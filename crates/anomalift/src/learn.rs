//! Cluster failures into patterns, and turn recurring ones into rules.
//!
//! Two thresholds, both there to stop the tool inventing advice:
//!
//! - a pattern needs **3+ occurrences** before it is a pattern at all. One-off
//!   errors are noise, and a rule written from one incident is superstition.
//! - it needs to appear in **2+ sessions**. Ten failures in a single afternoon is
//!   one bad afternoon; the same failure across separate sessions is a habit, and
//!   only habits are worth a standing instruction.
//!
//! Rule text is the weakest part of this design and is treated as such. For a
//! handful of failures whose fix is unambiguous there is a written remedy. For
//! everything else the tool states what recurred and leaves the wording to the
//! user rather than guessing at a cause it cannot see. A confidently wrong rule
//! in `CLAUDE.md` is worse than no rule, because it misleads the agent on every
//! future turn.

use std::collections::HashMap;

use crate::model::{Pattern, Session};
use crate::signature::signature;

pub const MIN_OCCURRENCES: usize = 3;
pub const MIN_SESSIONS: usize = 2;
const MAX_EXAMPLES: usize = 3;
/// Distinct user requests recorded per pattern. Enough to see a theme; few
/// enough that the store stays readable.
const MAX_QUERIES: usize = 5;

/// A known failure with a known fix.
struct Remedy {
    /// Matched case-insensitively against the signature.
    needle: &'static str,
    /// The signature must also start with one of these, so advice cannot leak
    /// across tools. Error phrasing is not unique to a program: `npm`, `node`
    /// and `cat` all say "no such file or directory", and `grep`, `tar` and
    /// `rg` all say "cannot be used together". Matching on the message alone
    /// wrote the `cd` rule for an npm failure and the `git show` rule for a
    /// tar failure - a confidently wrong line in CLAUDE.md, which this module
    /// exists to avoid.
    applies_to: &'static [&'static str],
    advice: &'static str,
}

/// Remedies are written only where the fix is unambiguous and general.
///
/// Two properties are required of every entry, and the second is the one that
/// is easy to skip: the advice must be correct *for the error the needle
/// actually matches*, not for the error it sounds like. A needle broad enough
/// to catch a whole tool's rejections will attach one fix to several unrelated
/// causes, and the majority case is then given advice written for a rare one.
///
/// The temptation is to add clever entries; resist it, because a rule that is
/// right in the observed case and wrong in general costs more than it saves.
const REMEDIES: &[Remedy] = &[
    Remedy {
        needle: "ambiguous argument",
        applies_to: &["Bash: fatal:", "Bash: exit"],
        advice: "Check a git ref exists (`git cat-file -e <ref>`) before `git show`/`git log`; \
                 an unknown revision fails with `exit 128: ambiguous argument`.",
    },
    Remedy {
        needle: "cannot be used together",
        applies_to: &["Bash: fatal:"],
        advice: "Do not combine `--name-only`/`--name-status` with `-s` in `git show`; \
                 `-s` suppresses the diff that `--name-only` asks for. Use \
                 `git show --name-only --format=` instead.",
    },
    Remedy {
        needle: "eisdir",
        applies_to: &["Read:"],
        advice: "Check a path is a file before reading it; use `ls` or `Glob` for directories.",
    },
    // Split deliberately. `Grep` reports every ripgrep rejection with the same
    // "ripgrep rejected the pattern, glob, or file type" preamble, so a needle
    // on the preamble matches an unknown `--type` value, an unknown flag and a
    // malformed regex alike - three causes with three different fixes. The
    // needles below match the cause, not the preamble.
    Remedy {
        needle: "unrecognized file type",
        applies_to: &["Grep:"],
        advice: "Pass ripgrep's own type names to `Grep`'s `type` argument, not file \
                 extensions: `ts` already covers `.tsx`, and `js` covers `.jsx`. \
                 `rg --type-list` is the full set; for anything not in it use \
                 `glob` (e.g. `*.tsx`) instead.",
    },
    Remedy {
        needle: "regex parse error",
        applies_to: &["Grep:"],
        advice: "Prefer literal search (`-F`, or `output_mode` with a plain string) and \
                 escape regex metacharacters (`{}`, `[]`, `+`) when searching for code.",
    },
    Remedy {
        needle: "no such file or directory",
        applies_to: &["Bash: (eval):cd:", "Bash: cd:"],
        advice: "Confirm a directory exists before `cd`; prefer absolute paths in `Bash`.",
    },
];

/// Group every failure across sessions into patterns, most frequent first.
pub fn patterns(sessions: &[Session]) -> Vec<Pattern> {
    struct Acc {
        tool: String,
        count: usize,
        sessions: std::collections::HashSet<String>,
        examples: Vec<String>,
        queries: Vec<String>,
        first: u64,
        last: u64,
    }
    let mut acc: HashMap<String, Acc> = HashMap::new();

    for session in sessions {
        // Walk queries rather than a flattened list of steps: the user request
        // in flight is the context a rule may need, and flattening discards it.
        for query in &session.queries {
            for step in query.failures() {
                let Some(err) = step.error.as_deref() else {
                    continue;
                };
                let Some(sig) = signature(&step.tool, err) else {
                    continue;
                };
                let e = acc.entry(sig).or_insert_with(|| Acc {
                    tool: step.tool.clone(),
                    count: 0,
                    sessions: Default::default(),
                    examples: Vec::new(),
                    queries: Vec::new(),
                    first: session.mtime,
                    last: session.mtime,
                });
                e.count += 1;
                e.sessions.insert(session.id.clone());
                e.first = e.first.min(session.mtime);
                e.last = e.last.max(session.mtime);
                if e.examples.len() < MAX_EXAMPLES {
                    let one_line = err.replace(['\n', '\r'], " ");
                    e.examples.push(one_line.chars().take(160).collect());
                }
                let q = summarize_query(&query.text);
                // Distinct only: one query that failed nine times would otherwise
                // fill the list and hide the pattern's actual spread.
                if !q.is_empty() && e.queries.len() < MAX_QUERIES && !e.queries.contains(&q) {
                    e.queries.push(q);
                }
            }
        }
    }

    let mut out: Vec<Pattern> = acc
        .into_iter()
        .map(|(signature, a)| Pattern {
            signature,
            tool: a.tool,
            count: a.count,
            sessions: a.sessions.len(),
            examples: a.examples,
            queries: a.queries,
            first_seen: a.first,
            last_seen: a.last,
        })
        .collect();
    // Ties broken by signature so output is stable across runs; an unstable
    // ordering would make the diff proposed to CLAUDE.md churn for no reason.
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    out
}

/// Reduce a user request to the part that identifies it.
///
/// Raw query text is dominated by scaffolding. Agent harnesses wrap the real
/// request in boilerplate - taking the first 120 characters of
/// "A change needs to be made to this repository, described below.
/// <description>..." yields the same string for every instance, so five
/// "distinct" queries turn out to be one prefix repeated.
///
/// So: prefer the contents of a `<description>`-style block when one exists,
/// drop tag lines, and take the first line with actual words in it.
fn summarize_query(text: &str) -> String {
    // Content of the first XML-ish block, which is where harnesses put the real
    // request. Matched generically rather than on a fixed tag name, since every
    // harness names it differently.
    let inner = text
        .find('<')
        .and_then(|open| {
            let after_tag = text[open..].find('>')? + open + 1;
            let close = text[after_tag..].find("</")? + after_tag;
            let body = text[after_tag..close].trim();
            (body.len() > 8).then(|| body.to_string())
        })
        .unwrap_or_else(|| text.to_string());

    let line = inner
        .lines()
        .map(str::trim)
        .find(|l| {
            !l.is_empty()
                && !l.starts_with('<')
                && !l.starts_with("```")
                && l.chars().any(char::is_alphabetic)
        })
        .unwrap_or("");

    line.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(100)
        .collect()
}

/// Patterns that clear both thresholds.
pub fn recurring(patterns: &[Pattern]) -> Vec<&Pattern> {
    patterns
        .iter()
        .filter(|p| p.count >= MIN_OCCURRENCES && p.sessions >= MIN_SESSIONS)
        .collect()
}

/// Share of all clustered failures that fall into recurring patterns.
///
/// Printed on every run so the premise of the tool stays checkable rather than
/// becoming folklore. Recurrence is a property of a particular history, not a
/// constant: if it collapses on a given machine, there is nothing here to
/// prevent and the tool says so instead of manufacturing rules.
pub fn recurrence_rate(patterns: &[Pattern]) -> f64 {
    let total: usize = patterns.iter().map(|p| p.count).sum();
    if total == 0 {
        return 0.0;
    }
    let rec: usize = recurring(patterns).iter().map(|p| p.count).sum();
    rec as f64 / total as f64
}

/// A `CLAUDE.md` line for a pattern, or `None` when the fix is not known.
///
/// Returning `None` is the honest outcome for most patterns. The tool reports
/// what recurred either way; it only writes a rule when it has one.
pub fn rule_for(p: &Pattern) -> Option<String> {
    let hay = p.signature.to_ascii_lowercase();
    let remedy = REMEDIES.iter().find(|r| {
        hay.contains(r.needle)
            && r.applies_to
                .iter()
                .any(|prefix| hay.starts_with(&prefix.to_ascii_lowercase()))
    })?;
    Some(format!(
        "{} ({}× in {} sessions)",
        remedy.advice, p.count, p.sessions
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Query, Step};

    fn session(id: &str, mtime: u64, errors: &[(&str, &str)]) -> Session {
        Session {
            id: id.into(),
            project: "p".into(),
            mtime,
            queries: vec![Query {
                idx: 0,
                text: "q".into(),
                steps: errors
                    .iter()
                    .enumerate()
                    .map(|(i, (tool, err))| Step {
                        idx: i,
                        tool: (*tool).into(),
                        target: None,
                        failed: true,
                        error: Some((*err).into()),
                    })
                    .collect(),
                tokens: 0,
                cost_usd: 0.0,
            }],
        }
    }

    #[test]
    fn one_bad_afternoon_is_not_a_habit() {
        // Five identical failures, all in one session: enough occurrences but
        // not enough sessions, so no rule is proposed.
        let s = session(
            "a",
            1,
            &[(
                "Read",
                "EISDIR: illegal operation on a directory, read '/x'",
            ); 5],
        );
        let pats = patterns(&[s]);
        assert_eq!(pats[0].count, 5);
        assert!(recurring(&pats).is_empty());
    }

    #[test]
    fn the_same_failure_across_sessions_is() {
        let e = (
            "Read",
            "EISDIR: illegal operation on a directory, read '/x'",
        );
        let sessions: Vec<_> = (0..3).map(|i| session(&i.to_string(), i, &[e])).collect();
        let pats = patterns(&sessions);
        let rec = recurring(&pats);
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].count, 3);
        assert!(rule_for(rec[0]).unwrap().contains("file before reading"));
    }

    #[test]
    fn query_summary_reaches_past_harness_boilerplate() {
        // The real case: five "distinct" queries were one identical prefix,
        // because the request lives inside the tag, not before it.
        let a = "A change needs to be made to this repository, described below.\n\n\
                 <description>\nfix the retry timeout in the payment client\n</description>\n\
                 Find which files to modify.";
        let b = "A change needs to be made to this repository, described below.\n\n\
                 <description>\nadd pagination to the orders list\n</description>\n\
                 Find which files to modify.";
        assert_eq!(
            summarize_query(a),
            "fix the retry timeout in the payment client"
        );
        assert_ne!(summarize_query(a), summarize_query(b));
    }

    #[test]
    fn query_summary_passes_plain_text_through() {
        assert_eq!(
            summarize_query("  why is the build failing?  "),
            "why is the build failing?"
        );
        assert_eq!(summarize_query(""), "");
    }

    #[test]
    fn contentless_failures_never_cluster() {
        let sessions: Vec<_> = (0..5)
            .map(|i| session(&i.to_string(), i, &[("Bash", "Exit code 1")]))
            .collect();
        assert!(patterns(&sessions).is_empty());
    }

    #[test]
    fn ripgrep_rejections_get_the_fix_for_their_own_cause() {
        // One preamble, several causes. An unknown `--type` value is by far the
        // most common, and it used to be handed the advice written for a
        // malformed regex - correct-sounding, and no help at all.
        let bad_type = (
            "Grep",
            "Search failed - ripgrep rejected the pattern, glob, or file type without \
             searching: rg: unrecognized file type: tsx",
        );
        let bad_regex = (
            "Grep",
            "Search failed - ripgrep rejected the pattern: rg: regex parse error: unclosed \
             character class",
        );
        for (err, expected) in [(bad_type, "--type-list"), (bad_regex, "escape regex")] {
            let sessions: Vec<_> = (0..3).map(|i| session(&i.to_string(), i, &[err])).collect();
            let pats = patterns(&sessions);
            let rule = rule_for(recurring(&pats)[0]).expect("a rule for a known cause");
            assert!(rule.contains(expected), "wrong remedy: {rule}");
        }
    }

    #[test]
    fn unknown_failures_get_no_invented_rule() {
        let e = ("Bash", "some novel failure nobody has a remedy for yet");
        let sessions: Vec<_> = (0..3).map(|i| session(&i.to_string(), i, &[e])).collect();
        let pats = patterns(&sessions);
        assert_eq!(recurring(&pats).len(), 1);
        assert_eq!(rule_for(recurring(&pats)[0]), None);
    }
}
