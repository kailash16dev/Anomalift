//! Live failure capture, from Claude Code's `PostToolUseFailure` hook.
//!
//! Everything else in this tool learns by re-reading transcripts after the
//! fact. That works, but it is lagged and lossy: a failure is only visible once
//! the session file has been flushed, and `source/claude.rs` has to reconstruct
//! which tool a `tool_result` belonged to by pairing ids across lines. The hook
//! hands us the same facts already paired, at the instant they happen.
//!
//! **The payload shape was read out of the binary, not guessed.** In
//! `~/.local/share/claude/versions/2.1.239` the zod schema for the event is:
//!
//! ```text
//! session_id, transcript_path, cwd, permission_mode?, agent_id?, agent_type?
//!   + hook_event_name: "PostToolUseFailure"
//!   + tool_name, tool_input, tool_use_id, error, is_interrupt?, duration_ms?
//! ```
//!
//! Two things there are worth stating because the obvious guess is wrong:
//!
//! - the error text is **`error`**, not `tool_error`. `tool_error` does appear
//!   as a string in the binary, but it belongs to telemetry
//!   (`tengu_advisor_tool_error`, `tool_error_categories`), not to this payload.
//!   `tool_error` is still accepted as a fallback so a rename in a future
//!   release degrades to a miss rather than silence.
//! - there is **no `tool_response`** on a failure. That field is `PostToolUse`
//!   only; a failed call has an `error` instead.
//!
//! **This code runs inside every failing tool call, so it may never fail
//! loudly.** A hook that exits non-zero, prints, or blocks degrades every
//! subsequent turn of the user's session - and it does so at the exact moment
//! the session is already going wrong. Every path here therefore ends in "write
//! nothing and exit 0": malformed JSON, absent fields, an unwritable store, a
//! payload from a different event. Silence is the designed behaviour, not a
//! swallowed bug. Even a panic is caught, because a panicking hook is a
//! non-zero exit.
//!
//! **Concurrency: the file is an append-only log, not a JSON array.** Several
//! Claude Code sessions fail at once routinely. Read-modify-write on a shared
//! JSON array loses records under that - two processes read the same array and
//! the second write erases the first. Locking is not a fix either: the honest
//! response to a contended lock in a hook is to give up (see above), so a lock
//! converts lost updates into lost records. A single `O_APPEND` write of one
//! short line is atomic on every platform this runs on, so appending is the one
//! approach that neither loses records nor can block. Deduplication into counts
//! then happens when the log is folded - on read, and during compaction - rather
//! than on the hot path.
//!
//! The cap is enforced by compaction rather than by refusing to write: the file
//! is rotated aside, folded to one record per (signature, command shape), and
//! the folded records appended back. See `compact` for the one record that can
//! be lost there and why that is the right trade.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::effect::opportunity_key;
use crate::signature::signature;
use crate::store::Store;

/// Stop reading stdin after this. A `tool_input` can carry a whole file's
/// contents, so the payload is not small, but it is not unbounded either and a
/// hook must not be the thing that exhausts memory.
const MAX_PAYLOAD: u64 = 4 * 1024 * 1024;

/// Give up on stdin after this and exit 0.
///
/// Claude Code always writes the payload and closes the pipe, so this never
/// fires in production. It exists because a blocked read is the one failure
/// mode that cannot be recovered from downstream: the tool call stays wedged
/// until the hook's own timeout, and the user watches it hang. Run by hand from
/// a terminal, this is also what stops `anomalift hook` sitting there forever.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Error text is kept for a human to read, not to match on - `signature` has
/// already extracted the part that identifies the failure.
const MAX_ERROR: usize = 500;
/// Matches the cap `source/claude.rs` applies to the same field, so a command
/// captured live and the same command mined from a transcript produce the same
/// opportunity key rather than two.
const MAX_TARGET: usize = 200;
const MAX_SESSION: usize = 64;
const MAX_CWD: usize = 200;

/// Compact once the log passes this. Roughly 1500-3000 raw lines.
const MAX_BYTES: u64 = 1024 * 1024;
/// Distinct failures kept after folding. Chosen so a full store is ~300KB,
/// comfortably under `MAX_BYTES` - otherwise every append past the cap would
/// trigger another compaction and the hot path would stop being cheap.
const MAX_RECORDS: usize = 500;

/// One observed failure, or - after folding - one class of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// From `signature::signature`, so live capture and transcript mining agree
    /// on identity. A second normaliser here would drift from the one that is
    /// tested against real data, and the two would disagree about what is the
    /// same bug.
    pub signature: String,
    pub tool: String,
    /// From `effect::opportunity_key`, for the same reason: this is the
    /// denominator `anomalift effect` measures against.
    pub opportunity_key: String,
    /// Verbatim (truncated). Normalisation lives in `signature`; the raw text is
    /// what a human reads to judge whether a rule is justified.
    pub error: String,
    pub session_id: String,
    pub cwd: String,
    /// Unix seconds. On a folded record, the most recent occurrence.
    pub at: u64,
    /// Occurrences folded into this record. Absent on freshly appended lines,
    /// which are worth one each.
    #[serde(default = "one")]
    pub count: u32,
}

fn one() -> u32 {
    1
}

/// Entry point for `anomalift hook`. Returns unit on every path by design.
pub fn run(rules_file: Option<PathBuf>) {
    // A panic is a non-zero exit and a message on the user's screen, which is
    // exactly what this must never do. Silence the default handler first so a
    // caught panic does not print a backtrace into the transcript.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(|| {
        let _ = capture(rules_file);
    });
    std::panic::set_hook(previous);
}

/// The whole job: read one payload, record it. `None` means "nothing recorded",
/// which is a normal outcome and never reported.
fn capture(rules_file: Option<PathBuf>) -> Option<()> {
    let text = read_stdin()?;
    let payload: Value = serde_json::from_str(&text).ok()?;
    let path = store_path(&payload, rules_file);
    record(&path, &payload, now())
}

/// Read stdin to EOF, bounded in both size and time.
fn read_stdin() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = Vec::new();
        let ok = (&mut stdin).take(MAX_PAYLOAD).read_to_end(&mut buf).is_ok();
        // Drain whatever is left instead of exiting on a full pipe. Stopping at
        // the cap would close stdin under a writer that is still writing, and
        // the EPIPE it gets back is an error surfaced in the user's session -
        // precisely the outcome this module exists to avoid. The bytes past the
        // cap are discarded, so memory stays bounded either way.
        let _ = std::io::copy(&mut stdin, &mut std::io::sink());
        let _ = tx.send(ok.then(|| String::from_utf8_lossy(&buf).into_owned()));
    });
    // The reader thread is deliberately abandoned on timeout. Returning from
    // `main` terminates the process and every thread with it, so a stuck read
    // cannot keep the hook alive past this point.
    rx.recv_timeout(READ_TIMEOUT).ok().flatten()
}

/// Where this project's observations live.
///
/// The payload carries the session's `cwd`, so the store lands beside the
/// project the failure happened in rather than wherever the hook process
/// happened to be started. A `cwd` that is not a directory is ignored rather
/// than created: a malformed payload must not scatter `.anomalift` directories
/// across the filesystem.
fn store_path(payload: &Value, rules_file: Option<PathBuf>) -> PathBuf {
    if let Some(f) = rules_file {
        return Store::observed_path_for(&f);
    }
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    Store::observed_path_for(&cwd.join("CLAUDE.md"))
}

/// Build the record, append it, and compact if the log has grown past its cap.
///
/// Separate from `capture` so tests can drive it without a real stdin.
pub(crate) fn record(path: &Path, payload: &Value, now: u64) -> Option<()> {
    let obs = observation(payload, now)?;
    // Compact *before* appending, so the new record cannot be the one lost in
    // the rotation window described on `compact`.
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let _ = compact(path);
    }
    append(path, &obs).ok()
}

/// Turn a hook payload into an observation, or `None` if there is nothing to
/// learn from it.
pub(crate) fn observation(payload: &Value, now: u64) -> Option<Observation> {
    // Refuse any event other than the one this understands. A `PostToolUse`
    // payload has a `tool_response` where this expects an `error`, and treating
    // a success as a failure would poison the counts silently.
    match payload.get("hook_event_name").and_then(Value::as_str) {
        Some("PostToolUseFailure") | None => {}
        Some(_) => return None,
    }
    // The user interrupting a call is not the agent's mistake. `signature`
    // filters the same thing by message text; this catches it by flag, which is
    // the version that does not depend on the wording of an error string.
    if payload.get("is_interrupt").and_then(Value::as_bool) == Some(true) {
        return None;
    }

    let tool = payload.get("tool_name").and_then(Value::as_str)?;
    if tool.is_empty() {
        return None;
    }
    // `error` is the real field; `tool_error` is accepted so a rename upstream
    // costs a version of coverage rather than all of it.
    let error = payload
        .get("error")
        .or_else(|| payload.get("tool_error"))
        .and_then(Value::as_str)?;

    // `None` here is the designed drop, not a parse failure: a contentless
    // `Exit code 1`, or a user rejection, teaches nothing and must not be
    // counted as a recurrence.
    let sig = signature(tool, error)?;

    let target = payload.get("tool_input").and_then(target_of);
    Some(Observation {
        signature: sig,
        tool: tool.to_string(),
        opportunity_key: opportunity_key(tool, target.as_deref()),
        error: clip(error, MAX_ERROR),
        session_id: payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(|s| clip(s, MAX_SESSION))
            .unwrap_or_default(),
        cwd: payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(|s| clip(s, MAX_CWD))
            .unwrap_or_default(),
        at: now,
        count: 1,
    })
}

/// What the tool was acting on. `tool_input` is a different object per tool.
///
/// The key order mirrors `source/claude.rs` exactly. It has to: the same call
/// seen live and seen in a transcript must yield the same target, or the same
/// failure would be counted under two opportunity keys and each would look half
/// as frequent as it is.
fn target_of(input: &Value) -> Option<String> {
    input
        .get("file_path")
        .or_else(|| input.get("command"))
        .or_else(|| input.get("pattern"))
        .and_then(Value::as_str)
        .map(|s| clip(s, MAX_TARGET))
}

/// Append one line. The only write on the hot path.
fn append(path: &Path, obs: &Observation) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(obs).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // One `write_all` of one short line. Every field is capped above precisely
    // so this stays a single small write, which `O_APPEND` places at the end of
    // the file atomically even with other processes appending concurrently.
    f.write_all(line.as_bytes())
}

/// Parse a log. Unreadable or corrupt lines are skipped, never fatal.
///
/// A half-written line is possible if a process was killed mid-append, and one
/// bad line must not discard everything learned before it.
pub(crate) fn read_log(path: &Path) -> Vec<Observation> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Observation>(l).ok())
        .collect()
}

/// Collapse repeats into counts.
///
/// The key is (signature, opportunity key) rather than the signature alone: the
/// same error arising from `git show` and from `git log` is one bug with two
/// causes, and `anomalift effect` needs both shapes to compute a denominator.
pub(crate) fn fold(records: &[Observation]) -> Vec<Observation> {
    let mut out: Vec<Observation> = Vec::new();
    let mut seen: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    for r in records {
        let key = (r.signature.clone(), r.opportunity_key.clone());
        match seen.get(&key) {
            Some(&i) => {
                let e: &mut Observation = &mut out[i];
                e.count = e.count.saturating_add(r.count);
                // Keep the most recent occurrence's context. An old cwd or
                // session id is less useful than the one it last happened in.
                if r.at >= e.at {
                    e.at = r.at;
                    e.session_id = r.session_id.clone();
                    e.cwd = r.cwd.clone();
                    e.error = r.error.clone();
                }
            }
            None => {
                seen.insert(key, out.len());
                out.push(r.clone());
            }
        }
    }
    out
}

/// Fold the log in place and drop the least interesting records.
///
/// Rotate-then-append rather than write-then-rename: renaming a rebuilt file
/// over the live path would erase anything appended while it was being built,
/// which is the lost-update bug this module exists to avoid. Rotating first
/// means concurrent writers create a fresh log and keep going.
///
/// One record can still be lost here - by a process that opened the file before
/// the rename and wrote after it. That window is the few microseconds of a
/// single append, it only opens when the cap is crossed, and the alternative
/// (locking) risks losing a record on *every* contended call. Losing at most
/// one observation per megabyte of log is the better trade.
fn compact(path: &Path) -> std::io::Result<()> {
    let rotated = path.with_extension(format!("compacting.{}", std::process::id()));
    std::fs::rename(path, &rotated)?;
    let mut folded = fold(&read_log(&rotated));
    // Keep habits over one-offs. Sorting by recency alone would evict a failure
    // that has happened two hundred times in favour of a novel one that has
    // happened once, and recurrence is the entire signal this tool looks for.
    folded.sort_by(|a, b| b.count.cmp(&a.count).then(b.at.cmp(&a.at)));
    folded.truncate(MAX_RECORDS);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for o in &folded {
        // One write per line, for the same atomicity reason as `append`: a
        // single large buffer could be split and interleaved with a concurrent
        // append, corrupting both.
        if let Ok(mut line) = serde_json::to_string(o) {
            line.push('\n');
            f.write_all(line.as_bytes())?;
        }
    }
    std::fs::remove_file(&rotated)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Truncate on a char boundary; `&str[..n]` panics mid-codepoint, and a panic
/// here would be a non-zero exit inside the user's session.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anomalift-hook-{}-{}-{name}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(".anomalift").join("observed.jsonl")
    }

    fn payload(tool: &str, error: &str, input: Value) -> Value {
        json!({
            "hook_event_name": "PostToolUseFailure",
            "session_id": "abc123",
            "transcript_path": "/Users/x/.claude/projects/p/s.jsonl",
            "cwd": "/Users/x/proj",
            "permission_mode": "default",
            "tool_name": tool,
            "tool_input": input,
            "tool_use_id": "toolu_01",
            "error": error,
        })
    }

    // --- the failure modes that must all end in "nothing written" ---

    #[test]
    fn malformed_json_records_nothing() {
        // The hook is fed by another program; a truncated pipe is a real
        // possibility and must not produce an error the user ever sees.
        for bad in ["", "{", "not json", "[1,2,3]", "null", "\u{0}"] {
            assert!(
                serde_json::from_str::<Value>(bad).ok().is_none() || {
                    let v: Value = serde_json::from_str(bad).unwrap();
                    observation(&v, 100).is_none()
                }
            );
        }
    }

    #[test]
    fn missing_fields_record_nothing() {
        assert!(observation(&json!({}), 100).is_none());
        // No tool name: nothing to key a signature on.
        assert!(observation(&json!({ "error": "fatal: not a git repository" }), 100).is_none());
        // No error: a failure with no message teaches nothing.
        assert!(observation(&json!({ "tool_name": "Bash" }), 100).is_none());
        // Right fields, wrong types.
        assert!(observation(&json!({ "tool_name": 7, "error": ["x"] }), 100).is_none());
        assert!(observation(
            &json!({ "tool_name": "", "error": "fatal: bad thing" }),
            100
        )
        .is_none());
    }

    #[test]
    fn a_different_hook_event_records_nothing() {
        // A PostToolUse payload has no `error`, but if one ever grows a field by
        // that name it must still not be counted as a failure.
        let mut p = payload(
            "Bash",
            "fatal: ambiguous argument 'x'",
            json!({ "command": "git show" }),
        );
        p["hook_event_name"] = json!("PostToolUse");
        assert!(observation(&p, 100).is_none());
    }

    #[test]
    fn an_unwritable_store_records_nothing_and_does_not_panic() {
        // A file where the directory should be: `create_dir_all` fails, and the
        // hook has to shrug rather than propagate.
        let base =
            std::env::temp_dir().join(format!("anomalift-hook-block-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join(".anomalift").join("observed.jsonl");
        let p = payload(
            "Bash",
            "fatal: ambiguous argument 'x': unknown revision",
            json!({}),
        );
        assert!(record(&path, &p, 100).is_none());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn contentless_and_user_caused_failures_are_dropped() {
        // Deferred to `signature`, and asserted here so a change there cannot
        // start writing noise into the store unnoticed.
        assert!(observation(&payload("Bash", "Exit code 1", json!({})), 100).is_none());
        let rejected = "The user doesn't want to proceed with this tool use.";
        assert!(observation(&payload("Bash", rejected, json!({})), 100).is_none());
    }

    #[test]
    fn an_interrupted_call_is_dropped() {
        // The user pressing escape is not a mistake to learn from, and the flag
        // says so without depending on the wording of the error text.
        let mut p = payload(
            "Bash",
            "fatal: ambiguous argument 'x': unknown revision",
            json!({}),
        );
        p["is_interrupt"] = json!(true);
        assert!(observation(&p, 100).is_none());
        p["is_interrupt"] = json!(false);
        assert!(observation(&p, 100).is_some());
    }

    // --- what it does record ---

    #[test]
    fn reuses_the_shared_signature_and_opportunity_key() {
        // The point of the module: no second normaliser. If these ever diverge,
        // a live capture and the same failure mined from a transcript become two
        // patterns and each looks half as frequent as it is.
        let err = "Exit code 128\nfatal: ambiguous argument '3796860c93': unknown revision";
        let p = payload(
            "Bash",
            err,
            json!({ "command": "cd /x && git show --stat 3796860c93" }),
        );
        let o = observation(&p, 100).unwrap();
        assert_eq!(o.signature, signature("Bash", err).unwrap());
        assert_eq!(o.opportunity_key, "Bash:git show");
        assert_eq!(o.session_id, "abc123");
        assert_eq!(o.cwd, "/Users/x/proj");
        assert_eq!(o.count, 1);
    }

    #[test]
    fn extracts_the_target_per_tool_shape() {
        let long_err = "EISDIR: illegal operation on a directory, read '/Users/x/proj/src'";
        let read = observation(
            &payload("Read", long_err, json!({ "file_path": "/a/b.rs" })),
            1,
        );
        assert_eq!(read.unwrap().opportunity_key, "Read");

        let grep = observation(
            &payload(
                "Grep",
                "rg: unrecognized flag --foo here",
                json!({ "pattern": "fn *(" }),
            ),
            1,
        );
        assert_eq!(grep.unwrap().opportunity_key, "Grep");

        // Bash is the one where the target changes the key.
        let bash = observation(
            &payload(
                "Bash",
                "fatal: not a git repository (or any parent)",
                json!({ "command": "git commit -m wip" }),
            ),
            1,
        );
        assert_eq!(bash.unwrap().opportunity_key, "Bash:git commit");

        // An unknown tool_input shape is not fatal; it just has no target.
        let none = observation(
            &payload(
                "Bash",
                "fatal: not a git repository (or any parent)",
                json!({ "todos": [] }),
            ),
            1,
        );
        assert_eq!(none.unwrap().opportunity_key, "Bash");
    }

    #[test]
    fn accepts_tool_error_as_a_fallback_field_name() {
        // Insurance against an upstream rename, not the field actually emitted.
        let p = json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Bash",
            "tool_error": "fatal: ambiguous argument 'x': unknown revision",
            "tool_input": { "command": "git show x" },
        });
        assert!(observation(&p, 100).is_some());
    }

    #[test]
    fn error_text_is_truncated() {
        let err = format!("fatal: {}", "é".repeat(2000));
        let o = observation(&payload("Bash", &err, json!({})), 1).unwrap();
        assert!(o.error.chars().count() <= MAX_ERROR);
        // Multibyte truncation must not have panicked or split a codepoint.
        assert!(o.error.starts_with("fatal: "));
    }

    #[test]
    fn a_recorded_failure_round_trips_through_the_log() {
        let path = tmp("rt");
        let err = "fatal: ambiguous argument 'x': unknown revision";
        assert!(record(
            &path,
            &payload("Bash", err, json!({ "command": "git show x" })),
            100
        )
        .is_some());
        let back = read_log(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].tool, "Bash");
        assert_eq!(back[0].opportunity_key, "Bash:git show");
        assert_eq!(back[0].count, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn repeats_fold_into_a_count() {
        let path = tmp("fold");
        let err = "fatal: ambiguous argument 'x': unknown revision";
        for t in 0..5 {
            record(
                &path,
                &payload("Bash", err, json!({ "command": "git show x" })),
                100 + t,
            );
        }
        // Different command shape, same error: a separate class, because the
        // denominator `anomalift effect` measures differs.
        record(
            &path,
            &payload("Bash", err, json!({ "command": "git log x" })),
            200,
        );

        let folded = fold(&read_log(&path));
        assert_eq!(folded.len(), 2);
        let show = folded
            .iter()
            .find(|o| o.opportunity_key == "Bash:git show")
            .unwrap();
        assert_eq!(show.count, 5);
        assert_eq!(show.at, 104, "the folded record keeps the latest timestamp");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn corrupt_lines_are_skipped_rather_than_discarding_the_file() {
        // A process killed mid-append leaves a partial line. One bad line must
        // not throw away everything learned before it.
        let path = tmp("corrupt");
        let err = "fatal: ambiguous argument 'x': unknown revision";
        record(
            &path,
            &payload("Bash", err, json!({ "command": "git show x" })),
            100,
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"signature\":\"Bash: trunc\n").unwrap();
        drop(f);
        record(
            &path,
            &payload(
                "Read",
                "EISDIR: illegal operation on a directory",
                json!({}),
            ),
            101,
        );
        assert_eq!(read_log(&path).len(), 2);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn compaction_bounds_the_file_and_keeps_the_habits() {
        let path = tmp("compact");
        // One failure that recurs, plus enough novel ones to blow past the cap.
        let hot = "fatal: ambiguous argument 'hot': unknown revision";
        let mut n = 0u64;
        while std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) <= MAX_BYTES {
            let err = format!("fatal: cannot open shard number {n} for reading, giving up");
            record(
                &path,
                &payload(
                    "Bash",
                    &err,
                    json!({ "command": format!("git cat-file {n}") }),
                ),
                n,
            );
            for _ in 0..3 {
                record(
                    &path,
                    &payload("Bash", hot, json!({ "command": "git show hot" })),
                    n,
                );
            }
            n += 1;
            assert!(n < 100_000, "log never reached the cap");
        }
        let before = std::fs::metadata(&path).unwrap().len();
        // The next record triggers compaction.
        record(
            &path,
            &payload("Bash", hot, json!({ "command": "git show hot" })),
            n,
        );

        let after = std::fs::metadata(&path).unwrap().len();
        assert!(
            after < before,
            "compaction did not shrink the log: {before} -> {after}"
        );
        let folded = fold(&read_log(&path));
        assert!(
            folded.len() <= MAX_RECORDS,
            "cap not enforced: {}",
            folded.len()
        );
        // The recurring failure is what the tool exists to find; it must survive
        // eviction even though it is older than the novel ones.
        let kept = folded
            .iter()
            .find(|o| o.opportunity_key == "Bash:git show")
            .unwrap();
        assert!(
            kept.count > 3,
            "the habit was evicted in favour of one-offs"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn the_store_lands_beside_the_project_the_failure_happened_in() {
        // The hook process's own cwd is not the session's cwd, so the payload's
        // is preferred - but only when it is real.
        let real = std::env::temp_dir();
        let p = json!({ "cwd": real.to_string_lossy() });
        assert_eq!(
            store_path(&p, None),
            real.join(".anomalift").join("observed.jsonl")
        );

        let bogus = json!({ "cwd": "/no/such/directory/anywhere" });
        let fallback = store_path(&bogus, None);
        assert!(fallback.ends_with(".anomalift/observed.jsonl"));
        assert!(!fallback.starts_with("/no/such"));
    }
}
