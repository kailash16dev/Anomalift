//! Terminal output.
//!
//! Colour is suppressed when stdout is not a terminal or `NO_COLOR` is set, so
//! piping into a file or another tool yields plain text. Getting this wrong is
//! the classic CLI defect: escape codes in a log make it unreadable and make
//! `grep` miss lines it should match.
//!
//! Two shaping decisions drive everything below.
//!
//! **Rows are labelled by what the user recognises.** The first version titled
//! each row with the normalised failure signature, which is the tool's internal
//! key and reads like a debug dump. A user thinks "my agent keeps screwing up
//! `git show`", not "`Bash: fatal: options '--name-only' ... cannot be used
//! together`". So the summary shows a command label and the signature moves to
//! `--all`. It is never dropped: the point of this tool is that a claim can be
//! checked, and a claim you cannot trace back to a verbatim error is worthless.
//!
//! **Nothing is claimed that the data does not carry.** Every figure printed
//! here reads an existing field. There is no estimate, no extrapolation, and no
//! improvement percentage for a verdict that is not `Works` - an earlier version
//! attributed 25,633 of 27,717 "tokens saved" to unproven patterns, and all of
//! it was noise.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::IsTerminal;

use crate::effect::opportunity_key;
use crate::learn::{recurrence_rate, recurring, rule_for, MIN_OCCURRENCES, MIN_SESSIONS};
use crate::model::{Pattern, Session};
use crate::signature::signature;

/// Command labels in the summary are clipped to this, so a long MCP tool name
/// cannot push the counts past 80 columns.
const LABEL_MAX: usize = 28;
/// Rows in the headline list. Enough to see the shape of the problem; a user
/// who wants the whole list has `--all`.
const TOP_N: usize = 5;
/// Field-name column in the `--all` evidence view, and the text width left over
/// once the 5-space indent and that column are taken out of 80.
const DETAIL_TAG: usize = 10;
const DETAIL_WIDTH: usize = 80 - 5 - DETAIL_TAG;

struct Style {
    on: bool,
}

impl Style {
    fn new() -> Self {
        Self {
            on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

/// Programs whose first argument is a subcommand rather than a filename.
///
/// `opportunity_key` keeps two leading words so `git show` and `git commit`
/// stay separate things to measure. For display, the second word is only useful
/// when it names an operation: `cat src/very/long/path.ts` should read `cat`,
/// not the path. Guessing from the shape of the word is not enough - `cat
/// readme` looks exactly like `git show` - so the decision is made by the
/// program, which is knowable, rather than by the argument, which is not.
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "git",
    "npm",
    "npx",
    "yarn",
    "pnpm",
    "bun",
    "cargo",
    "go",
    "docker",
    "kubectl",
    "gh",
    "pip",
    "brew",
    "poetry",
    "uv",
    "make",
    "terraform",
    "aws",
    "systemctl",
];

/// Programs a wrapper tool may name in its own error text.
///
/// Claude Code's `Grep` is a ripgrep wrapper, but only some of its failures come
/// from ripgrep - an `InputValidationError` is raised by the harness before
/// ripgrep is ever reached. So the name is used only when the pattern's own
/// error text contains it. That keeps the label something read out of the data
/// rather than a belief about the tool that would quietly rot when the wrapper
/// changes its implementation.
const NAMED_PROGRAMS: &[&str] = &[
    "ripgrep", "npm", "cargo", "pytest", "docker", "webpack", "eslint", "tsc",
];

/// `foo` appears in `hay` delimited by non-alphanumerics.
///
/// A plain `contains` would find "npm" inside a path segment like `.npmrc` and
/// label an unrelated failure. Both sides are already lowercase ASCII here.
fn contains_word(hay: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = hay[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = start == 0
            || !hay[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let after_ok = !hay[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// The program named in a pattern's error text, if it is one we recognise.
fn named_program(signature: &str) -> Option<&'static str> {
    let body = signature
        .split_once(": ")
        .map_or(signature, |(_, body)| body)
        .to_ascii_lowercase();
    NAMED_PROGRAMS
        .iter()
        .copied()
        .find(|p| contains_word(&body, p))
}

/// Human form of one `opportunity_key`, e.g. `Bash:git show` -> `git show`.
fn key_label(key: &str) -> String {
    let cmd = key.strip_prefix("Bash:").unwrap_or(key).trim();
    let mut words = cmd.split_whitespace();
    let Some(first) = words.next() else {
        return cmd.to_string();
    };
    // `/opt/homebrew/bin/rg` and `rg` are the same program to a reader.
    let head = first.rsplit('/').next().unwrap_or(first);
    match words.next() {
        Some(w)
            if SUBCOMMAND_PROGRAMS.contains(&head)
                && w.chars().all(|c| c.is_ascii_lowercase() || c == '-') =>
        {
            format!("{head} {w}")
        }
        _ => head.to_string(),
    }
}

/// Readable form of a tool name: `mcp__chrome__computer` -> `chrome computer`.
fn tool_label(tool: &str) -> String {
    let stripped = tool.strip_prefix("mcp__").unwrap_or(tool);
    if stripped.contains("__") {
        stripped.split("__").collect::<Vec<_>>().join(" ")
    } else {
        stripped.to_string()
    }
}

/// Every failure signature mapped to the label a user would recognise.
///
/// The label cannot be derived from `Pattern` alone: a pattern carries the
/// normalised error, not the command that produced it, and `git show` is what
/// the user recognises. So the commands are recovered from the sessions the
/// caller already loaded, using the very same `opportunity_key` that the effect
/// measurement uses for its denominator - label and denominator then always
/// describe the same set of calls, which they would not if this grew its own
/// notion of "the same command".
///
/// One pass, because the alternative (`baseline_for` per pattern) walks every
/// step once per signature, and a real run has hundreds of signatures.
fn labels(sessions: &[Session]) -> HashMap<String, String> {
    let mut counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for s in sessions {
        for step in s.failures() {
            let Some(err) = step.error.as_deref() else {
                continue;
            };
            let Some(sig) = signature(&step.tool, err) else {
                continue;
            };
            let label = step_label(&step.tool, step.target.as_deref(), &sig);
            *counts.entry(sig).or_default().entry(label).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|(sig, by_label)| {
            // A signature can arise from several commands. Ties break
            // alphabetically so two runs over the same data never disagree.
            let label = by_label
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(l, _)| l.clone())
                .unwrap_or_default();
            (sig, label)
        })
        .collect()
}

/// The label one failing step contributes.
fn step_label(tool: &str, target: Option<&str>, sig: &str) -> String {
    if tool != "Bash" {
        return named_program(sig)
            .map(str::to_string)
            .unwrap_or_else(|| tool_label(tool));
    }
    // `opportunity_key` deliberately strips a leading `cd … &&`, so `cd x &&
    // cat y` is measured as `cat` - correct for the denominator, since the
    // `cat` is the operation being repeated. But when the error *is* the `cd`,
    // titling the row `cat` points the user at the half of the command that
    // worked. So the label, and only the label, looks back at the prefix the
    // key threw away. `cd:` rather than a bare `cd` because a diagnostic
    // prefixed with the failing utility's name is the POSIX convention, and a
    // bare match would fire on any path containing a `cd` segment.
    if sig.contains("cd:") && target.is_some_and(|t| t.trim_start().starts_with("cd ")) {
        return "cd".to_string();
    }
    key_label(&opportunity_key(tool, target))
}

/// Signature -> human label, for callers outside this module.
///
/// Offered so `--json` and the MCP cache can carry the same label the terminal
/// shows; two independently derived labels for one pattern would be a way for
/// the JSON and the screen to disagree about what the user is looking at.
/// Whole-corpus rather than per-pattern because the label comes from the
/// commands in the sessions, not from the pattern in isolation.
#[allow(dead_code)] // Wired up by main.rs, which is not this change's to edit.
pub fn label_map(sessions: &[Session]) -> HashMap<String, String> {
    labels(sessions)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Pad to `width` by codepoints, never truncating.
///
/// `format!("{:<w$}")` already counts characters rather than bytes, but callers
/// pad strings that may carry colour escapes; those must be padded on the plain
/// text or every coloured column drifts by the length of the escape sequence.
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - n))
}

/// `failures / attempts` as a percentage, or an em dash when there is no
/// denominator. A zero-attempt rate printed as `0%` reads as perfection; it
/// actually means nothing was measured, which is the confounder this whole tool
/// exists to avoid.
fn fmt_rate(failures: usize, attempts: usize) -> String {
    if attempts == 0 {
        "—".to_string()
    } else {
        format!("{:.1}%", failures as f64 / attempts as f64 * 100.0)
    }
}

/// Relative fall in failure rate, whole percent. Callers must only reach this
/// for a `Works` verdict.
fn fmt_improvement(before_rate: f64, after_rate: f64) -> Option<String> {
    if before_rate <= 0.0 || after_rate >= before_rate {
        return None;
    }
    let pct = (before_rate - after_rate) / before_rate * 100.0;
    Some(format!("{}%", pct.round() as u64))
}

/// Break text onto lines of at most `width` codepoints, indenting every line
/// after the first by `indent` spaces.
///
/// Used only in the evidence view, where clipping would be the wrong trade: a
/// truncated error message is exactly the part a user needs to judge whether a
/// rule is justified, and "the raw error is reachable" is not true if the tail
/// of it is thrown away. Words longer than `width` (a stack frame, a URL) are
/// hard-broken rather than allowed to overflow into an 81st column.
fn wrap(s: &str, width: usize, indent: usize) -> Vec<String> {
    let width = width.max(8);
    let pad = " ".repeat(indent);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in s.split_whitespace() {
        let mut word: &str = word;
        while word.chars().count() > width {
            if cur_len > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            let head: String = word.chars().take(width).collect();
            let taken = head.len();
            lines.push(head);
            word = &word[taken..];
        }
        let wlen = word.chars().count();
        if cur_len == 0 {
            cur.push_str(word);
            cur_len = wlen;
        } else if cur_len + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + wlen;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_len = wlen;
        }
    }
    if cur_len > 0 {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    for l in lines.iter_mut().skip(1) {
        *l = format!("{pad}{l}");
    }
    lines
}

/// Thousands separators, so a five-figure token count is legible at a glance.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

// ---------------------------------------------------------------------------
// `anomalift` / `anomalift learn`
// ---------------------------------------------------------------------------

pub fn report(sessions: &[Session], patterns: &[Pattern], show_all: bool) {
    print!(
        "{}",
        render_report(&Style::new(), sessions, patterns, show_all)
    );
}

/// Rendered as a string rather than printed line by line so the exact output is
/// testable. A renderer that can only be checked by eye is one that regresses.
fn render_report(st: &Style, sessions: &[Session], patterns: &[Pattern], show_all: bool) -> String {
    let mut o = String::new();
    let total_failures: usize = sessions.iter().map(|s| s.failures().count()).sum();
    let clustered: usize = patterns.iter().map(|p| p.count).sum();
    let rec = recurring(patterns);
    let rate = recurrence_rate(patterns);

    let _ = writeln!(o, "\n  {}", st.bold("Anomalift"));

    // Empty state one: nothing to read. Not a crash, and it must not look like
    // one - a first run before the user has any transcripts is the single most
    // likely way this program is ever executed.
    if sessions.is_empty() {
        let _ = writeln!(o, "\n  No agent transcripts found in ~/.claude/projects.\n");
        let _ = writeln!(
            o,
            "  {}",
            st.dim("Anomalift learns from sessions you have already run.")
        );
        let _ = writeln!(
            o,
            "  {}\n",
            st.dim("Use your agent for a while, then run this again.")
        );
        return o;
    }

    let _ = writeln!(
        o,
        "  {}",
        st.dim(&format!(
            "{} · {} · {}",
            plural(sessions.len(), "session", "sessions"),
            plural(total_failures, "failed tool call", "failed tool calls"),
            plural(clustered, "with a readable error", "with a readable error"),
        ))
    );

    // Empty state two: failures happened, but none carried a message worth
    // clustering. Saying so plainly beats manufacturing advice from noise.
    if patterns.is_empty() {
        let _ = writeln!(o, "\n  {}", st.green("Nothing recurring to learn from."));
        let _ = writeln!(
            o,
            "\n  {}",
            st.dim("Failures with no message are excluded - they teach nothing.")
        );
        let _ = writeln!(o, "  {}\n", st.dim("Run this again after more sessions."));
        return o;
    }

    let with_rules = rec.iter().filter(|p| rule_for(p).is_some()).count();
    let pct = (rate * 100.0).round() as u32;

    // Empty state three: patterns exist but none has cleared the thresholds.
    // The user needs to know this is a "not yet", not a "nothing here", so the
    // thresholds are stated rather than left implicit.
    if rec.is_empty() {
        let _ = writeln!(
            o,
            "\n  No failure has repeated often enough to act on yet.\n"
        );
        let _ = writeln!(
            o,
            "    {} distinct failure patterns seen",
            st.bold(&patterns.len().to_string())
        );
        let _ = writeln!(
            o,
            "    {}",
            st.dim(&format!(
                "a pattern needs {MIN_OCCURRENCES}+ occurrences across {MIN_SESSIONS}+ sessions"
            ))
        );
        let _ = writeln!(o, "\n  Run:");
        let _ = writeln!(
            o,
            "    {}       {}",
            st.bold("anomalift --all"),
            st.dim("Show every pattern, with the raw error")
        );
        let _ = writeln!(o);
        return o;
    }

    let _ = writeln!(o, "\n  Your agent has repeated failures.\n");
    let _ = writeln!(
        o,
        "    {} of readable failures are recurring",
        st.bold(&format!("{pct}%"))
    );
    let _ = writeln!(
        o,
        "    {} recurring patterns found",
        st.bold(&rec.len().to_string())
    );
    if with_rules > 0 {
        let _ = writeln!(
            o,
            "    {} {} a known fix",
            st.bold(&with_rules.to_string()),
            if with_rules == 1 { "has" } else { "have" }
        );
    } else {
        // Rule 3: never dress this up. A tool that has found problems it cannot
        // fix should say so, not imply a fix exists behind another command.
        let _ = writeln!(o, "    {}", st.yellow("none have a known fix"));
    }

    let map = labels(sessions);
    let top: Vec<&Pattern> = rec.iter().take(TOP_N).copied().collect();
    let width = top
        .iter()
        .map(|p| label_for(&map, p).chars().count().min(LABEL_MAX))
        .max()
        .unwrap_or(0)
        .max(12);

    let _ = writeln!(o, "\n  Top opportunities:");
    for p in &top {
        let label = clip(&label_for(&map, p), LABEL_MAX);
        let count = format!("{} failures", p.count);
        let note = match rule_for(p) {
            Some(_) => st.green("fix known"),
            None => st.dim("no known fix"),
        };
        let _ = writeln!(
            o,
            "    {}  {}  {}",
            st.bold(&pad(&label, width)),
            pad(&count, 13),
            note
        );
    }
    if rec.len() > top.len() {
        let _ = writeln!(
            o,
            "    {}",
            st.dim(&format!("+ {} more", rec.len() - top.len()))
        );
    }

    let _ = writeln!(o, "\n  Run:");
    if with_rules > 0 {
        let _ = writeln!(
            o,
            "    {}      {}",
            st.bold("anomalift apply"),
            st.dim("Apply proven fixes to your agent")
        );
    }
    let _ = writeln!(
        o,
        "    {}      {}",
        st.bold("anomalift --all"),
        st.dim("Show every pattern, with the raw error")
    );
    if with_rules == 0 {
        let _ = writeln!(
            o,
            "\n  {}",
            st.dim("No rule is proposed: these patterns have no remedy the tool is sure of.")
        );
    }
    let _ = writeln!(o);

    if show_all {
        o.push_str(&render_detail(st, patterns, &map));
    }
    o
}

/// The checkable view: raw signature and a verbatim error for every pattern.
///
/// This exists because the summary above is a claim, and a claim the user
/// cannot trace to the text their agent actually saw is not evidence. It is
/// verbose on purpose.
fn render_detail(st: &Style, patterns: &[Pattern], map: &HashMap<String, String>) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "  {}\n", st.bold("Every pattern"));
    for p in patterns {
        let recurs = p.count >= MIN_OCCURRENCES && p.sessions >= MIN_SESSIONS;
        let head = format!(
            "{}  {} in {}",
            pad(&clip(&label_for(map, p), LABEL_MAX), LABEL_MAX),
            plural(p.count, "failure", "failures"),
            plural(p.sessions, "session", "sessions"),
        );
        let _ = writeln!(
            o,
            "  {}{}",
            if recurs {
                st.bold(&head)
            } else {
                st.dim(&head)
            },
            if recurs {
                String::new()
            } else {
                st.dim("  (below threshold)")
            }
        );
        // Wrapped, not clipped. See `wrap`: a truncated error is the part the
        // user needed in order to check the claim.
        let field = |tag: &str, text: &str, colour: &dyn Fn(&str) -> String| {
            let mut s = String::new();
            for (i, line) in wrap(&text.replace(['\n', '\r'], " "), DETAIL_WIDTH, 0)
                .into_iter()
                .enumerate()
            {
                let head = if i == 0 {
                    pad(tag, DETAIL_TAG)
                } else {
                    " ".repeat(DETAIL_TAG)
                };
                let _ = writeln!(s, "     {}{}", st.dim(&head), colour(&line));
            }
            s
        };
        o.push_str(&field("signature", &p.signature, &|t| st.dim(t)));
        if let Some(ex) = p.examples.first() {
            o.push_str(&field("example", ex, &|t| t.to_string()));
        }
        match rule_for(p) {
            Some(rule) => o.push_str(&field("fix", &rule, &|t| st.green(t))),
            None => o.push_str(&field(
                "fix",
                "no known fix - review this one yourself",
                &|t| st.dim(t),
            )),
        }
        let _ = writeln!(o);
    }
    o
}

fn label_for(map: &HashMap<String, String>, p: &Pattern) -> String {
    map.get(&p.signature)
        .cloned()
        .unwrap_or_else(|| tool_label(&p.tool))
}

// ---------------------------------------------------------------------------
// `anomalift effect`
// ---------------------------------------------------------------------------

/// Before/after report for `anomalift effect`.
///
/// "No evidence" is printed in the same weight and position as a success. The
/// whole reason this command computes rates rather than counts is that an unused
/// tool otherwise looks cured, and a quiet caveat would let that read as a win
/// anyway.
pub fn effect_report(sessions: &[Session], effects: &[crate::effect::Effect], since: &str) {
    print!("{}", render_effect(&Style::new(), sessions, effects, since));
}

fn render_effect(
    st: &Style,
    sessions: &[Session],
    effects: &[crate::effect::Effect],
    since: &str,
) -> String {
    use crate::effect::{Verdict, TOKENS_PER_FAILURE};

    let mut o = String::new();
    let _ = writeln!(o, "\n  {}", st.bold("Anomalift · effect"));

    if effects.is_empty() {
        let _ = writeln!(o, "\n  No failure patterns to measure.\n");
        let _ = writeln!(
            o,
            "  {}\n",
            st.dim("Run `anomalift apply` first, then come back once the agent has run again.")
        );
        return o;
    }

    // The headline is the only number a user actually wants from this command,
    // so it leads. It sums `tokens_saved`, which is zero for anything that is
    // not proven - the total therefore cannot be inflated by a hopeful verdict.
    let proven: Vec<&crate::effect::Effect> = effects
        .iter()
        .filter(|e| e.verdict == Verdict::Works)
        .collect();
    let tokens: u64 = proven.iter().map(|e| e.tokens_saved()).sum();
    let avoided: f64 = proven.iter().map(|e| e.failures_avoided()).sum();

    let _ = writeln!(
        o,
        "  {}",
        st.dim(&format!(
            "{} measured · baseline {}",
            plural(sessions.len(), "session", "sessions"),
            since
        ))
    );

    if tokens > 0 {
        let _ = writeln!(
            o,
            "\n  {}",
            st.bold(&st.green(&format!("~{} tokens saved", thousands(tokens))))
        );
        let _ = writeln!(
            o,
            "  {}",
            st.dim(&format!(
                "from {} · ~{:.0} failures avoided",
                plural(proven.len(), "proven fix", "proven fixes"),
                avoided
            ))
        );
        // Split across two lines: the caveat matters more than compactness,
        // but an 80-column terminal wraps it into an unreadable mess.
        let _ = writeln!(
            o,
            "  {}",
            st.dim(&format!(
                "at {TOKENS_PER_FAILURE} tokens per failure — a median measured"
            ))
        );
        let _ = writeln!(
            o,
            "  {}",
            st.dim("on one machine's history, not calibrated to yours")
        );
    } else {
        let _ = writeln!(o, "\n  {}", st.bold("No proven savings yet."));
        let _ = writeln!(
            o,
            "  {}",
            st.dim("Nothing is counted for an unproven or untested result.")
        );
    }

    let map = labels(sessions);

    // Split off the patterns that had nothing before the cutoff. They are
    // untestable rather than interesting, and on a real run there are hundreds
    // of them - printing each in full buries the handful that carry a verdict.
    // Note what is *not* filtered: a pattern that failed before and was then
    // never attempted again stays in the list at full size, because that is
    // exactly the case a reader would otherwise mistake for a cure.
    let mut shown: Vec<&crate::effect::Effect> = Vec::new();
    let mut untestable = 0usize;
    for e in effects {
        if e.before_failures == 0 && e.after_failures == 0 {
            continue;
        }
        if e.before_attempts == 0 {
            untestable += 1;
            continue;
        }
        shown.push(e);
    }

    // Proven first, then anything that got worse, then unproven, then untested.
    // Within a group the biggest baseline leads.
    shown.sort_by_key(|e| {
        let rank = match e.verdict {
            Verdict::Works => 0,
            Verdict::Failed => 1,
            Verdict::Unproven => 2,
            Verdict::NoEvidence => 3,
        };
        (rank, std::cmp::Reverse(e.before_failures))
    });

    if shown.is_empty() {
        let _ = writeln!(
            o,
            "\n  {}",
            st.dim("No pattern has a baseline to measure against yet.")
        );
        o.push_str(&untestable_note(st, untestable));
        let _ = writeln!(o);
        return o;
    }

    let _ = writeln!(o);
    for e in &shown {
        let label = map
            .get(&e.signature)
            .cloned()
            .unwrap_or_else(|| tool_label(&e.tool));
        let _ = writeln!(o, "  {}", st.bold(&clip(&label, LABEL_MAX)));

        // Align the two fractions on the slash so the eye reads the change, not
        // the digits.
        let wf = e
            .before_failures
            .max(e.after_failures)
            .to_string()
            .chars()
            .count();
        let wa = e
            .before_attempts
            .max(e.after_attempts)
            .to_string()
            .chars()
            .count();
        let _ = writeln!(
            o,
            "      Before  {:>wf$} / {:>wa$}   {:>6}",
            e.before_failures,
            e.before_attempts,
            fmt_rate(e.before_failures, e.before_attempts),
        );
        let _ = writeln!(
            o,
            "      After   {:>wf$} / {:>wa$}   {:>6}",
            e.after_failures,
            e.after_attempts,
            fmt_rate(e.after_failures, e.after_attempts),
        );
        let _ = writeln!(o);

        // Every verdict is bold. `NO EVIDENCE` is not allowed to be quieter
        // than `WORKS`: an abandoned tool shows zero failures and reads as a
        // cure unless the caveat is as loud as the claim would have been.
        let (mark, name) = match e.verdict {
            Verdict::Works => (st.green("✓"), st.bold(&st.green("WORKS"))),
            Verdict::Unproven => (st.yellow("~"), st.bold(&st.yellow("UNPROVEN"))),
            Verdict::NoEvidence => (st.yellow("?"), st.bold(&st.yellow("NO EVIDENCE"))),
            Verdict::Failed => (st.red("✗"), st.bold(&st.red("NO IMPROVEMENT"))),
        };
        let _ = writeln!(o, "      {mark} {name}");

        match e.verdict {
            // Rule: an improvement percentage is only ever printed here. Any
            // other verdict means the fall is indistinguishable from chance, and
            // a percentage would be the tool marking its own homework.
            Verdict::Works => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(p) = fmt_improvement(e.before_rate(), e.after_rate()) {
                    parts.push(format!("~{p} fewer failures"));
                }
                parts.push(format!("~{:.0} failures avoided", e.failures_avoided()));
                parts.push(format!("~{} tokens saved", thousands(e.tokens_saved())));
                let _ = writeln!(o, "      {}", st.green(&parts.join(" · ")));
            }
            Verdict::NoEvidence if e.after_attempts == 0 => {
                let _ = writeln!(
                    o,
                    "      {}",
                    st.dim("the agent never tried this again — the rule was never tested")
                );
            }
            Verdict::NoEvidence => {
                let _ = writeln!(
                    o,
                    "      {}",
                    st.dim("not enough on both sides of the cutoff to compare")
                );
            }
            Verdict::Unproven => {
                let _ = writeln!(
                    o,
                    "      {}",
                    st.dim("the fall is too small to tell from chance at this sample size")
                );
            }
            Verdict::Failed => {
                let _ = writeln!(
                    o,
                    "      {}",
                    st.dim("the failure rate did not fall after the rule was applied")
                );
            }
        }
        let _ = writeln!(o);
    }

    o.push_str(&untestable_note(st, untestable));
    o
}

/// Patterns with nothing before the cutoff are untestable, not interesting.
///
/// They are counted rather than listed because a real run produced 333 of them
/// and printing each in full buried the handful that carried a verdict. Note
/// what is *not* summarised away: a pattern that failed before and was never
/// attempted again keeps its full entry, since that is precisely the case a
/// reader would otherwise mistake for a cure.
fn untestable_note(st: &Style, untestable: usize) -> String {
    if untestable == 0 {
        return String::new();
    }
    let mut o = String::new();
    let _ = writeln!(
        o,
        "  {}",
        st.dim(&format!(
            "{untestable} more had no attempts before the cutoff — nothing to compare."
        ))
    );
    let _ = writeln!(
        o,
        "  {}\n",
        st.dim("`anomalift effect --json` lists them all.")
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{Effect, Verdict};
    use crate::model::{Query, Step};

    fn plain() -> Style {
        Style { on: false }
    }

    #[test]
    fn clip_is_codepoint_safe() {
        assert_eq!(clip("abc", 10), "abc");
        assert_eq!(clip("ééééé", 3).chars().count(), 3);
    }

    #[test]
    fn no_color_disables_escapes() {
        // Style reads the env at construction; assert the plain branch is plain.
        let plain = Style { on: false };
        assert_eq!(plain.red("x"), "x");
        assert!(Style { on: true }.red("x").contains('\x1b'));
    }

    #[test]
    fn pad_counts_codepoints_not_bytes() {
        // "é" is two bytes; padding by byte length would produce a short column
        // and every row after it would sit one space to the left.
        assert_eq!(pad("éé", 4).chars().count(), 4);
        assert_eq!(pad("abcdef", 3), "abcdef");
    }

    #[test]
    fn key_label_keeps_a_subcommand_but_drops_an_argument() {
        assert_eq!(key_label("Bash:git show"), "git show");
        assert_eq!(key_label("Bash:npm install"), "npm install");
        // `find <path>` and `cat <file>`: the second word is an argument, and a
        // path in a summary line is noise.
        assert_eq!(key_label("Bash:find /Users/x/projects/app"), "find");
        assert_eq!(key_label("Bash:cat SidePanelPageLayout"), "cat");
        assert_eq!(key_label("Bash:grep \"agent\""), "grep");
        // A program that does not take subcommands keeps only its own name,
        // even when the argument happens to look like one.
        assert_eq!(key_label("Bash:cat readme"), "cat");
        // Full paths to a binary read as the binary.
        assert_eq!(key_label("Bash:/opt/homebrew/bin/rg foo"), "rg");
        // Non-Bash keys are already the tool name.
        assert_eq!(key_label("Read"), "Read");
    }

    #[test]
    fn key_label_is_multibyte_safe() {
        assert_eq!(key_label("Bash:échec ééé"), "échec");
        assert_eq!(key_label("Bash:"), "");
    }

    #[test]
    fn tool_label_unwraps_mcp_names() {
        assert_eq!(
            tool_label("mcp__claude-in-chrome__computer"),
            "claude-in-chrome computer"
        );
        assert_eq!(tool_label("Read"), "Read");
    }

    #[test]
    fn named_program_needs_a_word_boundary() {
        // The real case this serves: Grep is a ripgrep wrapper, and its ripgrep
        // failures should say so.
        assert_eq!(
            named_program("Grep: Search failed — ripgrep rejected the pattern"),
            Some("ripgrep")
        );
        // But not every Grep failure comes from ripgrep, and a substring match
        // inside a path must never rename an unrelated pattern.
        assert_eq!(
            named_program("Grep: <tool_use_error>InputValidationError"),
            None
        );
        assert_eq!(named_program("Read: ENOENT open '/x/.npmrc'"), None);
        assert!(contains_word("npm err! code e404", "npm"));
        assert!(!contains_word("gonpmgo", "npm"));
    }

    #[test]
    fn labels_come_from_the_commands_that_failed() {
        let s = session(
            1,
            vec![
                step("Bash", Some("git show --name-only -s HEAD"), Some(GIT_ERR)),
                step(
                    "Bash",
                    Some("git show --name-status -s HEAD"),
                    Some(GIT_ERR),
                ),
                step("Read", Some("/tmp/dir"), Some(EISDIR)),
            ],
        );
        let map = labels(&[s]);
        let git = map
            .iter()
            .find(|(k, _)| k.contains("cannot be used together"))
            .map(|(_, v)| v.clone());
        assert_eq!(git.as_deref(), Some("git show"));
        let read = map
            .iter()
            .find(|(k, _)| k.contains("EISDIR"))
            .map(|(_, v)| v.clone());
        assert_eq!(read.as_deref(), Some("Read"));
    }

    #[test]
    fn a_failed_cd_is_labelled_cd_not_the_command_after_it() {
        // Real data: `cd packages/x && cat y` where the `cd` is what failed.
        // The measurement key is `cat` on purpose; the label must not be, or
        // the row points at the half of the command that worked.
        let err = "Exit code 1\n(eval):cd:1: no such file or directory: packages/twenty-front/src";
        let sig = crate::signature::signature("Bash", err).unwrap();
        assert_eq!(
            step_label(
                "Bash",
                Some("cd packages/twenty-front/src && cat a.ts"),
                &sig
            ),
            "cd"
        );
        // The prefix is only consulted when the error names `cd`; an unrelated
        // failure inside the same command shape still reads as the command.
        let other =
            crate::signature::signature("Bash", "Exit code 1\ncat: a.ts: permission denied")
                .unwrap();
        assert_eq!(
            step_label("Bash", Some("cd /tmp && cat a.ts"), &other),
            "cat"
        );
    }

    #[test]
    fn rate_with_no_denominator_is_not_zero_percent() {
        // 0/0 printed as "0%" reads as a perfect score; it means nothing was
        // measured, which is the confounder the whole tool exists to avoid.
        assert_eq!(fmt_rate(0, 0), "—");
        assert_eq!(fmt_rate(19, 129), "14.7%");
        assert_eq!(fmt_rate(1, 84), "1.2%");
    }

    #[test]
    fn improvement_is_relative_and_refuses_non_improvements() {
        let before = 19.0 / 129.0;
        let after = 1.0 / 84.0;
        assert_eq!(fmt_improvement(before, after).as_deref(), Some("92%"));
        assert_eq!(fmt_improvement(0.1, 0.1), None);
        assert_eq!(fmt_improvement(0.1, 0.2), None);
        assert_eq!(fmt_improvement(0.0, 0.0), None);
    }

    #[test]
    fn thousands_separates() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(27717), "27,717");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    // -- fixtures ----------------------------------------------------------

    const GIT_ERR: &str =
        "Exit code 128\nfatal: options '--name-only' and '-s' cannot be used together";
    const EISDIR: &str = "EISDIR: illegal operation on a directory, read '/Users/x/src'";

    fn step(tool: &str, target: Option<&str>, err: Option<&str>) -> Step {
        Step {
            idx: 0,
            tool: tool.into(),
            target: target.map(str::to_string),
            failed: err.is_some(),
            error: err.map(str::to_string),
        }
    }

    fn session(mtime: u64, steps: Vec<Step>) -> Session {
        Session {
            id: format!("s{mtime}"),
            project: "p".into(),
            mtime,
            queries: vec![Query {
                idx: 0,
                text: "q".into(),
                steps,
                tokens: 0,
                cost_usd: 0.0,
            }],
        }
    }

    fn pattern(sig: &str, tool: &str, count: usize, sessions: usize) -> Pattern {
        Pattern {
            signature: sig.into(),
            tool: tool.into(),
            count,
            sessions,
            examples: vec!["verbatim error text".into()],
            queries: vec![],
            first_seen: 0,
            last_seen: 0,
        }
    }

    fn effect(
        sig: &str,
        tool: &str,
        bf: usize,
        ba: usize,
        af: usize,
        aa: usize,
        v: Verdict,
    ) -> Effect {
        Effect {
            signature: sig.into(),
            tool: tool.into(),
            before_failures: bf,
            before_attempts: ba,
            after_failures: af,
            after_attempts: aa,
            p_value: 0.001,
            verdict: v,
        }
    }

    // -- rendering ---------------------------------------------------------

    #[test]
    fn no_transcripts_does_not_read_like_a_crash() {
        let out = render_report(&plain(), &[], &[], false);
        assert!(out.contains("Anomalift"));
        assert!(out.contains("No agent transcripts found"));
        assert!(out.contains("then run this again"));
        assert!(!out.contains("error"));
        assert!(out.lines().all(|l| l.chars().count() <= 80));
    }

    #[test]
    fn failures_but_nothing_clusters_is_stated_plainly() {
        let s = session(1, vec![step("Bash", Some("ls"), Some("Exit code 1"))]);
        let out = render_report(&plain(), &[s], &[], false);
        assert!(out.contains("Nothing recurring to learn from"));
        assert!(out.contains("Failures with no message are excluded"));
    }

    #[test]
    fn patterns_below_threshold_read_as_not_yet() {
        let s = session(1, vec![step("Bash", Some("git show"), Some(GIT_ERR))]);
        let p = pattern("Bash: fatal: cannot be used together", "Bash", 1, 1);
        let out = render_report(&plain(), &[s], &[p], false);
        assert!(out.contains("No failure has repeated often enough to act on yet"));
        assert!(out.contains("3+ occurrences across 2+ sessions"));
        // No apply suggestion: there is nothing to apply.
        assert!(!out.contains("anomalift apply"));
    }

    #[test]
    fn recurring_without_a_fix_says_so_rather_than_hiding_it() {
        let s = session(
            1,
            vec![step(
                "Bash",
                Some("weirdtool run"),
                Some("Exit code 1\nsome novel failure nobody has a remedy for"),
            )],
        );
        let p = pattern(
            "Bash: some novel failure nobody has a remedy for",
            "Bash",
            4,
            3,
        );
        let out = render_report(&plain(), &[s], &[p], false);
        assert!(out.contains("none have a known fix"));
        assert!(out.contains("no known fix"));
        assert!(out.contains("no remedy the tool is sure of"));
        // Nothing to apply, so nothing is offered.
        assert!(!out.contains("anomalift apply"));
    }

    #[test]
    fn the_headline_names_commands_not_signatures() {
        let s = session(
            1,
            vec![
                step("Bash", Some("git show --name-only -s HEAD"), Some(GIT_ERR)),
                step("Read", Some("/Users/x/src"), Some(EISDIR)),
            ],
        );
        let sig_git = crate::signature::signature("Bash", GIT_ERR).unwrap();
        let sig_read = crate::signature::signature("Read", EISDIR).unwrap();
        let pats = vec![
            pattern(&sig_git, "Bash", 19, 14),
            pattern(&sig_read, "Read", 11, 10),
        ];
        let out = render_report(&plain(), &[s], &pats, false);

        assert!(out.contains("Your agent has repeated failures."));
        assert!(out.contains("Top opportunities:"));
        assert!(out.contains("git show"));
        assert!(out.contains("19 failures"));
        assert!(out.contains("2 have a known fix"));
        assert!(out.contains("anomalift apply"));
        // The signature is a debug key; it must not be the row title.
        assert!(!out.contains("cannot be used together"));
        assert!(out.lines().all(|l| l.chars().count() <= 80), "{out}");
    }

    #[test]
    fn all_keeps_the_signature_and_a_verbatim_error_reachable() {
        // The claim has to stay checkable. `--all` is where the evidence lives.
        let s = session(
            1,
            vec![step("Bash", Some("git show --name-only -s"), Some(GIT_ERR))],
        );
        let sig = crate::signature::signature("Bash", GIT_ERR).unwrap();
        let mut p = pattern(&sig, "Bash", 19, 14);
        p.examples = vec![GIT_ERR.replace('\n', " ")];
        let out = render_report(&plain(), &[s], &[p], true);
        assert!(out.contains("Every pattern"));
        assert!(out.contains("signature"));
        assert!(out.contains("example"));
        // Every word of the raw signature and the verbatim example survives -
        // wrapped across lines, never truncated. Clipping the tail of an error
        // would remove exactly the part a user reads to check the claim.
        for word in sig.split_whitespace().chain(GIT_ERR.split_whitespace()) {
            assert!(out.contains(word), "missing {word:?} in\n{out}");
        }
        assert!(!out.contains('…'), "evidence must not be elided:\n{out}");
        assert!(out.lines().all(|l| l.chars().count() <= 80), "{out}");
    }

    #[test]
    fn wrap_never_overflows_and_never_loses_a_word() {
        let s = "fatal: options '--name-only' and '-s' cannot be used together";
        let lines = wrap(s, 30, 4);
        assert!(lines.iter().all(|l| l.chars().count() <= 34));
        let joined = lines
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, s);
        // Continuation lines carry the indent; the first does not.
        assert!(!lines[0].starts_with(' '));
        assert!(lines[1].starts_with("    "));
    }

    #[test]
    fn wrap_hard_breaks_an_unbreakable_token_on_a_codepoint() {
        // A path or stack frame longer than the column must not overflow, and
        // must not be split mid-codepoint.
        let long = "é".repeat(50);
        let lines = wrap(&long, 12, 0);
        assert!(lines.iter().all(|l| l.chars().count() <= 12));
        assert_eq!(lines.concat().chars().count(), 50);
        assert_eq!(wrap("", 10, 0), vec![String::new()]);
    }

    #[test]
    fn effect_leads_with_tokens_saved() {
        let s = session(
            1,
            vec![step("Bash", Some("git show --name-only -s"), Some(GIT_ERR))],
        );
        let sig = crate::signature::signature("Bash", GIT_ERR).unwrap();
        let e = effect(&sig, "Bash", 19, 129, 1, 84, Verdict::Works);
        let out = render_effect(&plain(), &[s], &[e], "frozen baselines");
        assert!(out.contains("tokens saved"));
        assert!(out.contains("git show"));
        assert!(out.contains("Before  19 / 129    14.7%"), "{out}");
        assert!(out.contains("After    1 /  84     1.2%"), "{out}");
        assert!(out.contains("✓ WORKS"));
        assert!(out.contains("~92% fewer failures"));
        assert!(out.lines().all(|l| l.chars().count() <= 80), "{out}");
    }

    #[test]
    fn an_abandoned_tool_never_reads_as_a_cure() {
        // The regression this guards: failures go to zero because the agent
        // stopped using the tool. It must be as loud as a success and claim
        // nothing at all.
        let s = session(
            1,
            vec![step("Bash", Some("git show --name-only -s"), Some(GIT_ERR))],
        );
        let sig = crate::signature::signature("Bash", GIT_ERR).unwrap();
        let e = effect(&sig, "Bash", 19, 129, 0, 0, Verdict::NoEvidence);
        let out = render_effect(&plain(), &[s], &[e], "frozen baselines");
        assert!(out.contains("NO EVIDENCE"));
        assert!(out.contains("never tried this again"));
        // 0/0 is not 0%.
        assert!(out.contains("After    0 /   0        —"), "{out}");
        // No saving, no percentage, no headline.
        assert!(out.contains("No proven savings yet."));
        assert!(!out.contains("fewer failures"));
        assert!(!out.contains("tokens saved"));
    }

    #[test]
    fn no_evidence_is_as_prominent_as_works() {
        // Same weight, same position, same indent - checked structurally so a
        // future edit cannot quietly demote one of them.
        let s = session(1, vec![step("Bash", Some("git show -s"), Some(GIT_ERR))]);
        let sig = crate::signature::signature("Bash", GIT_ERR).unwrap();
        let st = Style { on: true };
        let works = render_effect(
            &st,
            std::slice::from_ref(&s),
            &[effect(&sig, "Bash", 9, 40, 1, 40, Verdict::Works)],
            "x",
        );
        let none = render_effect(
            &st,
            &[s],
            &[effect(&sig, "Bash", 9, 40, 0, 0, Verdict::NoEvidence)],
            "x",
        );
        let bold_line = |out: &str, needle: &str| {
            out.lines()
                .find(|l| l.contains(needle))
                .map(|l| (l.starts_with("      "), l.contains("\x1b[1m")))
        };
        assert_eq!(bold_line(&works, "WORKS"), Some((true, true)));
        assert_eq!(bold_line(&none, "NO EVIDENCE"), Some((true, true)));
    }

    #[test]
    fn unproven_and_failed_never_claim_a_percentage() {
        // The 25,633-of-27,717 bug: unproven patterns claimed almost all of a
        // saving that did not exist.
        let s = session(1, vec![step("Bash", Some("git show -s"), Some(GIT_ERR))]);
        let sig = crate::signature::signature("Bash", GIT_ERR).unwrap();
        for v in [Verdict::Unproven, Verdict::Failed, Verdict::NoEvidence] {
            let out = render_effect(
                &plain(),
                std::slice::from_ref(&s),
                &[effect(&sig, "Bash", 9, 40, 4, 30, v)],
                "x",
            );
            assert!(
                !out.contains('%') || !out.contains("fewer failures"),
                "{out}"
            );
            assert!(!out.contains("tokens saved"), "{out}");
        }
    }

    #[test]
    fn effect_with_nothing_to_measure_is_not_a_crash() {
        let out = render_effect(&plain(), &[], &[], "frozen baselines");
        assert!(out.contains("No failure patterns to measure."));
        assert!(out.lines().all(|l| l.chars().count() <= 80));
    }

    #[test]
    fn untestable_patterns_are_counted_not_dumped() {
        // A real run produced 333 of these; printing each in full buried the
        // handful that carried a verdict.
        let s = session(1, vec![step("Bash", Some("git show -s"), Some(GIT_ERR))]);
        let sig = crate::signature::signature("Bash", GIT_ERR).unwrap();
        let effects: Vec<Effect> = (0..12)
            .map(|i| {
                effect(
                    &format!("{sig}{i}"),
                    "Bash",
                    0,
                    0,
                    1,
                    3,
                    Verdict::NoEvidence,
                )
            })
            .collect();
        let out = render_effect(&plain(), &[s], &effects, "2026-08-01");
        assert!(
            out.contains("12 more had no attempts before the cutoff"),
            "{out}"
        );
        assert!(out.contains("--json"));
    }
}
