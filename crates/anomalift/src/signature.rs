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
//! 2. Failures the agent was never allowed to make are not the agent's to learn
//!    from. A rejected tool call means the user said no; a permission block
//!    means the harness said no, before the tool ran. Neither is a mistake the
//!    agent could have avoided by working differently, so a rule written from
//!    either would be teaching the agent from decisions that were not its own.
//!
//! A third problem is not about dropping but about *choosing*: a failed `Bash`
//! call carries everything the command printed, and the failure is often not at
//! the head of it. See [`ERROR_MARKERS`].

/// Error text below this length after normalising is treated as contentless.
const MIN_SIGNAL: usize = 12;
/// Signatures are truncated so that a long traceback still groups by its head.
const SIG_LEN: usize = 95;

/// Errors that are not the agent's mistake and must never become a rule.
///
/// Two kinds, and the second was found only once the tool ran on someone else's
/// machine: the user declining a call, and the harness refusing one. A refusal
/// clusters perfectly - it is the same sentence every time - so it climbs the
/// ranking on merit it does not have. `cat` reached third place at four
/// failures that way, all of them the classifier saying no.
const NOT_AGENT_FAULT: &[&str] = &[
    "the user doesn't want to proceed",
    "user rejected",
    "request interrupted by user",
    "operation cancelled",
    // "Permission for this action was denied by the Claude Code auto mode
    // classifier. Reason: Blocked". Matched on the decision rather than on
    // "Blocked", which a real failure is entitled to say - a blocked port, a
    // push blocked by a branch protection rule.
    "permission for this action was denied",
    // The same refusal in its other spelling, "<tool_use_error>Blocked: sleep
    // 60 followed by: ...". The tag is what makes it a harness verdict instead
    // of a program reporting that something was blocked.
    "<tool_use_error>blocked:",
];

/// Phrases that mark a line as a complaint rather than as program output.
///
/// A `Bash` result is the command's whole output, and a command that prints
/// normally and then exits non-zero signs as its own stdout: a `du` listing, a
/// file echoed to the terminal, a `diff` hunk. Worse, the real complaint is
/// often at the tail, behind output that succeeded - `node -v`, then `npm -v`,
/// then `cargo: command not found` - where truncation to a fixed width never
/// reaches it.
///
/// There is no general test for "this line is an error", so this is an explicit
/// list of what tools actually say when they fail, in the spirit of
/// [`BOILERPLATE`]. A line matching nothing here is not judged: the whole text
/// is kept, because a noisy signature is something a human can still read,
/// while a dropped failure is invisible.
///
/// `warning:` is deliberately absent. A script that prints the same deprecation
/// notice on every run and then fails differently each time would otherwise
/// cluster all of its failures under the notice - which is what happened, 8
/// failures across 4 sessions, the largest cluster on that machine.
const ERROR_MARKERS: &[&str] = &[
    "error:",
    "exception:",
    "fatal:",
    "panicked at",
    "traceback (most recent call last)",
    "command not found",
    // cmd.exe and PowerShell's spelling of the same thing: "'uv' is not
    // recognized as an internal or external command" / "as the name of a
    // cmdlet".
    "is not recognized as",
    "no such file or directory",
    "permission denied",
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

    let body = strip_exit_prefix(error);
    // The marked line first, the whole capture only if that yields nothing: a
    // bare `error:` on its own line falls under MIN_SIGNAL, and dropping the
    // failure over it would be worse than signing it with the surrounding text.
    let head = error_line(body)
        .and_then(condense)
        .or_else(|| condense(body))?;

    let mut sig = String::with_capacity(tool.len() + 2 + head.len());
    sig.push_str(tool);
    sig.push_str(": ");
    sig.push_str(&head);
    Some(sig)
}

/// Normalise, strip the preamble, and refuse what is left if it says nothing.
fn condense(text: &str) -> Option<String> {
    let normalised = normalise(text);
    // Boilerplate is stripped *after* normalising so the comparison sees one
    // spelling: the same preamble reaches this point with an em dash or a
    // hyphen, and wrapped across a newline or not, depending on the client.
    let trimmed = strip_boilerplate(normalised.trim());
    if trimmed.len() < MIN_SIGNAL {
        // "Exit code N" and friends: a failure happened, but nothing about it is
        // learnable. Counting these would inflate recurrence with noise.
        return None;
    }
    Some(clip(trimmed, SIG_LEN).to_string())
}

/// The line carrying the failure, out of everything the command printed.
///
/// The *last* marked line wins, for two reasons the real data agrees on. The
/// complaint comes after the output that preceded it, so anything earlier is
/// what worked. And in a Python traceback the last marked line is the exception
/// itself, whose text differs per bug, while the `Traceback (most recent call
/// last):` head is identical for every one - picking the head would merge every
/// failure of a script into a single cluster with a single useless rule.
///
/// `None` when nothing usable is marked, leaving the caller the whole text.
fn error_line(error: &str) -> Option<&str> {
    let mut chosen = None;
    for line in error.lines() {
        let line = line.trim();
        // A line that ends where a structure opens is the *head* of the
        // message, not the message: `AskUserQuestion` fails with
        // "<tool_use_error>InputValidationError: [" and then a pretty-printed
        // JSON body, so picking it signs every validation error that tool ever
        // raises as one bucket and discards the `"code": "too_big"` a rule
        // would have to match on. The whole capture, flattened, keeps it.
        if line.ends_with(['[', '{', '(']) {
            continue;
        }
        if ERROR_MARKERS.iter().any(|m| contains_ignore_case(line, m)) {
            chosen = Some(line);
        }
    }
    chosen
}

/// Case-insensitive `contains` for an ASCII needle.
///
/// Lowercasing each line instead would allocate once per line of output, and
/// the output here is whole `du` and `diff` runs.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
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
    fn drops_harness_permission_blocks() {
        // Verbatim from the first machine that was not mine: three separate
        // clusters, one of them (`cat`) third-largest at four failures. The
        // command never ran, so there is no habit here to correct.
        let classifier = "Permission for this action was denied by the Claude Code \
                          auto mode classifier. Reason: Blocked";
        assert_eq!(signature("Bash", classifier), None);
        assert_eq!(signature("Edit", classifier), None);
        assert_eq!(
            signature(
                "Bash",
                "<tool_use_error>Blocked: sleep 60 followed by: tail -30 /tmp/build.log",
            ),
            None
        );
    }

    #[test]
    fn the_word_blocked_alone_is_still_a_failure() {
        // The exclusion is on the permission decision, not on the vocabulary:
        // a real error is allowed to say something was blocked.
        assert!(signature("Bash", "error: push blocked by branch protection").is_some());
        assert!(signature("Bash", "fatal: port 5432 blocked by firewall").is_some());
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
    fn the_error_at_the_tail_beats_the_output_in_front_of_it() {
        // Real cluster: `Bash: vN.N.N --- N.N.N --- PATH: line N: cargo:
        // command not found`. Two lines that worked, then the one that did not.
        let sig = signature(
            "Bash",
            "v22.14.0\n---\n10.9.2\n---\n/c/Users/dev/.bashrc: line 12: cargo: command not found",
        )
        .unwrap();
        assert_eq!(sig, "Bash: PATH: line N: cargo: command not found");
    }

    #[test]
    fn identical_output_with_different_failures_no_longer_merges() {
        // Real cluster: `Bash: === converter.py (root) === """ converter.py -
        // CLI shim...`, a script echoed to the terminal. Every failure of that
        // script shared the first screenful, so every failure was one cluster.
        let banner = "=== converter.py (root) ===\n\
                      \"\"\"converter.py - CLI shim forwarding to the real converter package.\"\"\"\n\
                      import sys, pathlib, argparse\n\
                      Traceback (most recent call last):\n  File \"converter.py\", line 3\n";
        let missing = format!("{banner}ModuleNotFoundError: No module named 'fitz'");
        let bad_amount = format!("{banner}ValueError: could not convert string to float");

        // The captures are identical for the whole signature width, so before
        // the tail was preferred these clipped to one signature.
        assert_eq!(
            clip(&normalise(&missing), SIG_LEN),
            clip(&normalise(&bad_amount), SIG_LEN)
        );

        let a = signature("Bash", &missing).unwrap();
        let b = signature("Bash", &bad_amount).unwrap();
        assert_ne!(a, b);
        assert!(a.contains("ModuleNotFoundError"), "{a}");
        assert!(b.contains("ValueError"), "{b}");
    }

    #[test]
    fn a_leading_warning_does_not_displace_the_error() {
        // The largest cluster on that machine - 8 failures over 4 sessions -
        // signed as this notice, clipped mid-word at the signature width. The
        // script prints it on every run and then fails differently each time,
        // so unrelated bugs collected under a line that is not an error at all.
        let warning = "warning: The `fitz` API is deprecated and will be removed in future. \
                       Use `import pymupdf` instead.";
        let missing = format!(
            "{warning}\nTraceback (most recent call last):\n  \
             File \"<string>\", line 1, in <module>\n\
             FileNotFoundError: [Errno 2] No such file or directory: 'invoice.pdf'"
        );
        let bad_amount = format!(
            "{warning}\nTraceback (most recent call last):\n  \
             File \"<string>\", line 1, in <module>\n\
             ValueError: cannot parse amount '1,234.00'"
        );

        let a = signature("Bash", &missing).unwrap();
        let b = signature("Bash", &bad_amount).unwrap();
        assert!(!a.contains("deprecated"), "{a}");
        assert!(!a.contains("Traceback"), "{a}");
        assert!(a.contains("FileNotFoundError"), "{a}");
        assert!(b.contains("ValueError"), "{b}");
        assert_ne!(a, b);
    }

    #[test]
    fn a_structure_opening_is_not_the_message() {
        // `AskUserQuestion` reports a schema violation as one marked line and
        // then a pretty-printed body. Picking the line puts every validation
        // error it ever raises in one bucket and throws away the part a rule
        // matches on, so the flattened capture is preferred instead.
        let err = "<tool_use_error>InputValidationError: [\n  {\n    \"origin\": \"array\",\n    \
                   \"code\": \"too_big\",\n    \"maximum\": 4,\n    \"path\": [\"questions\"]\n  }\n\
                   ]</tool_use_error>";
        let sig = signature("AskUserQuestion", err).unwrap();
        assert!(sig.contains("\"code\": \"too_big\""), "{sig}");
    }

    #[test]
    fn a_short_but_complete_error_line_still_wins() {
        // The rule above is about lines that stop where a structure opens, not
        // about short lines: a complete one-line complaint is still the
        // signature, not the output printed around it.
        let sig = signature(
            "Bash",
            "Checking repository state\nfatal: not a git repository\nrun `git init` first",
        )
        .unwrap();
        assert_eq!(sig, "Bash: fatal: not a git repository");
    }

    #[test]
    fn output_with_no_complaint_in_it_is_kept() {
        // `du` exiting non-zero after printing normally. Nothing in it is
        // marked as an error and guessing would drop a real failure, so the
        // signature stays noisy on purpose - a human can still read it.
        let du = "1.2M\t./venv/Lib\n4.0K\t./venv/Scripts\n1.2M\t./venv/Lib/site-packages";
        assert!(signature("Bash", du).is_some());
    }

    #[test]
    fn clip_does_not_panic_on_multibyte() {
        let long = "é".repeat(400);
        let _ = signature("Bash", &long);
    }
}
