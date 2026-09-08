//! Turning raw error text into a stable failure signature.
//!
//! This is the core of the tool and the place it is most likely to be wrong.
//! Normalise too aggressively and unrelated failures merge into one confident,
//! bogus rule; too weakly and nothing clusters, so nothing is ever learned.
//!
//! How well it clusters is a property of a particular history, so the tool
//! reports the resulting recurrence rate on every run rather than asserting a
//! figure here. No rate is claimed as general.
//!
//! Two rules govern what is *dropped*, both of which matter more than the
//! clustering itself:
//!
//! 1. A failure with no message carries no lesson. `Exit code 1` on its own is
//!    typically the largest single bucket and would generate a rule that says
//!    nothing. It is excluded rather than clustered.
//! 2. Failures caused by the user, not the agent, are not the agent's to learn
//!    from. A rejected tool call means the user said no; turning that into a
//!    standing instruction would teach the agent from the user's own choices.

/// Error text below this length after normalising is treated as contentless.
const MIN_SIGNAL: usize = 12;
/// Signatures are truncated so that a long traceback still groups by its head.
const SIG_LEN: usize = 95;

/// Errors that are not the agent's mistake and must never become a rule.
const NOT_AGENT_FAULT: &[&str] = &[
    "the user doesn't want to proceed",
    "user rejected",
    "request interrupted by user",
    "operation cancelled",
];

/// Normalise one failure into a signature, or `None` when it teaches nothing.
///
/// `None` is a deliberate outcome, not a parse failure: most noise in this data
/// is well-formed and meaningless.
pub fn signature(tool: &str, error: &str) -> Option<String> {
    let lower = error.to_ascii_lowercase();
    if NOT_AGENT_FAULT.iter().any(|p| lower.contains(p)) {
        return None;
    }

    let normalised = normalise(strip_exit_prefix(error));
    // Boilerplate is stripped *after* normalising so the comparison sees one
    // spelling: the same preamble reaches this point with an em dash or a
    // hyphen, and wrapped across a newline or not, depending on the client.
    let trimmed = strip_boilerplate(normalised.trim());
    if trimmed.len() < MIN_SIGNAL {
        // "Exit code N" and friends: a failure happened, but nothing about it is
        // learnable. Counting these would inflate recurrence with noise.
        return None;
    }

    let mut sig = String::with_capacity(tool.len() + 2 + SIG_LEN);
    sig.push_str(tool);
    sig.push_str(": ");
    sig.push_str(clip(trimmed, SIG_LEN));
    Some(sig)
}

/// Drop a leading `Exit code N` so the real message becomes the signature head.
///
/// Without this, every Bash failure starts with the same eleven characters and
/// truncation to a fixed width would merge unrelated errors.
fn strip_exit_prefix(error: &str) -> &str {
    let t = error.trim_start();
    let rest = match t.strip_prefix("Exit code ") {
        Some(r) => r,
        None => return t,
    };
    let after_digits = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    after_digits.trim_start_matches(['\n', ' ', ':', '\r'])
}

/// Fixed wrappers a tool puts in front of the message it was given.
///
/// Each is constant across every occurrence, so it carries no information, and
/// each is long enough to push the part that does past the truncation width.
/// Kept as an explicit list rather than a heuristic: guessing at where the
/// boilerplate ends would cut real messages short.
const BOILERPLATE: &[&str] = &[
    "Search failed - ripgrep rejected the pattern, glob, or file type without searching:",
    "Search failed - ripgrep rejected the pattern:",
    "Search failed -",
];

/// Drop a known constant preamble so the cause becomes the signature head.
///
/// The same reason as [`strip_exit_prefix`], with more at stake: `Grep`
/// announces every ripgrep rejection with 82 characters of identical text, and
/// the cause - an unknown file type, an unknown flag, a malformed regex - comes
/// after it. Truncated to a fixed width, all three collapse into one signature,
/// and any rule written for that signature is right about at most one of them.
fn strip_boilerplate(error: &str) -> &str {
    let t = error.trim_start();
    for prefix in BOILERPLATE {
        // `get` rather than an index: the prefix length can land mid-codepoint
        // in an error that happens to start with multibyte text, and slicing
        // there panics.
        if t.get(..prefix.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(prefix))
        {
            return t[prefix.len()..].trim_start();
        }
    }
    t
}

/// Replace the parts of an error that vary between occurrences of the same bug.
///
/// Paths, hashes and numbers are the arguments; what remains is the complaint.
/// `git show <sha>` failing on two different commits is one problem, not two.
fn normalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // An absolute or relative path: everything to the next separator.
            '/' => {
                out.push_str("PATH");
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || n == ':' || n == '\'' || n == '"' || n == ',' {
                        break;
                    }
                    chars.next();
                }
            }
            c if c.is_ascii_hexdigit() => {
                let mut run = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_hexdigit() {
                        run.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // A long hex run is a SHA; a short one is just a number or word.
                if run.len() >= 7 && run.chars().any(|c| c.is_ascii_alphabetic()) {
                    out.push_str("SHA");
                } else if run.chars().all(|c| c.is_ascii_digit()) {
                    out.push('N');
                } else {
                    out.push_str(&run);
                }
            }
            c if c.is_ascii_digit() => {
                out.push('N');
                while chars.peek().is_some_and(|n| n.is_ascii_digit()) {
                    chars.next();
                }
            }
            '\n' | '\r' | '\t' => out.push(' '),
            // Dash spelling is a client-rendering detail, not part of the
            // error. Folding it here keeps one failure from clustering as two.
            '\u{2014}' | '\u{2013}' | '\u{2011}' => out.push('-'),
            c => out.push(c),
        }
    }
    // Collapse runs of spaces so wrapping differences do not split a signature.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        let is_space = c == ' ';
        if !(is_space && prev_space) {
            collapsed.push(c);
        }
        prev_space = is_space;
    }
    collapsed
}

/// Truncate on a char boundary; `&str[..n]` panics mid-codepoint.
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_contentless_exit_codes() {
        // The largest bucket in the real data - 40 of 160 - and worth nothing.
        assert_eq!(signature("Bash", "Exit code 1"), None);
        assert_eq!(signature("Bash", "Exit code 127\n"), None);
    }

    #[test]
    fn drops_user_rejections() {
        // The user declining a tool call is not an agent mistake, and a rule
        // learned from it would train the agent on the user's own decisions.
        let e = "The user doesn't want to proceed with this tool use.";
        assert_eq!(signature("ExitPlanMode", e), None);
    }

    #[test]
    fn same_bug_different_arguments_groups() {
        // Two real git failures differing only by revision must be one pattern.
        let a = signature(
            "Bash",
            "Exit code 128\nfatal: ambiguous argument '3796860c93': unknown revision",
        );
        let b = signature(
            "Bash",
            "Exit code 128\nfatal: ambiguous argument 'a91f22de01': unknown revision",
        );
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn different_bugs_stay_separate() {
        let git = signature(
            "Bash",
            "Exit code 128\nfatal: ambiguous argument 'x': unknown revision",
        );
        let rg = signature(
            "Grep",
            "Search failed - ripgrep rejected the pattern: rg: unrecognized flag",
        );
        assert_ne!(git, rg);
    }

    #[test]
    fn paths_normalise_but_the_complaint_survives() {
        let a = signature(
            "Read",
            "EISDIR: illegal operation on a directory, read '/Users/x/proj/src'",
        );
        let b = signature(
            "Read",
            "EISDIR: illegal operation on a directory, read '/tmp/other/dir'",
        );
        assert_eq!(a, b);
        assert!(a.unwrap().contains("EISDIR"));
    }

    #[test]
    fn tool_name_separates_identical_text() {
        // The same message from two tools is two problems with two fixes.
        let a = signature("Bash", "no such file or directory: packages/web");
        let b = signature("Read", "no such file or directory: packages/web");
        assert_ne!(a, b);
    }

    #[test]
    fn constant_preambles_do_not_hide_the_cause() {
        // Grep prefixes every ripgrep rejection with the same 82 characters.
        // Left in place they consume the whole signature width, so an unknown
        // file type and a malformed regex become one pattern with one rule.
        let pre = "Search failed - ripgrep rejected the pattern, glob, or file type \
                   without searching: ";
        let bad_type = signature("Grep", &format!("{pre}rg: unrecognized file type: tsx")).unwrap();
        let bad_regex = signature(
            "Grep",
            &format!("{pre}rg: regex parse error: unclosed group"),
        )
        .unwrap();
        assert_ne!(bad_type, bad_regex);
        assert!(bad_type.contains("unrecognized file type"), "{bad_type}");
        assert!(bad_regex.contains("regex parse error"), "{bad_regex}");
    }

    #[test]
    fn clip_does_not_panic_on_multibyte() {
        let long = "é".repeat(400);
        let _ = signature("Bash", &long);
    }
}
