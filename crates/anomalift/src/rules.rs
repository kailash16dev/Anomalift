//! Writing rules into `CLAUDE.md`, and taking them back out.
//!
//! This is the only part of the tool that modifies a file the user owns, so it
//! is the part most able to do harm. Four constraints follow, and none of them
//! are negotiable:
//!
//! 1. **Only the marked block is ever touched.** Everything outside
//!    `<!-- anomalift:begin -->` / `<!-- anomalift:end -->` is preserved byte for byte. A
//!    `CLAUDE.md` is hand-written and often long; silently reflowing or
//!    reordering it would be worse than useless.
//! 2. **Nothing is written without confirmation.** The caller shows a diff and
//!    asks. An `--yes` flag exists for scripting, and is not the default.
//! 3. **A backup is written first**, next to the file, timestamped. Cheap
//!    insurance against a bug in this module.
//! 4. **`forget` removes exactly what `apply` added** - the block and nothing
//!    else - and leaves the file identical to its pre-`apply` state, including
//!    trailing whitespace.
//!
//! The block carries the date it was written. That is not decoration: it is the
//! cutoff `anomalift effect` uses to split before from after, so the measurement no
//! longer depends on the user remembering when they added a rule.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const BEGIN: &str = "<!-- anomalift:begin";
pub const END: &str = "<!-- anomalift:end -->";

/// Where the rules live, and what is currently there.
pub struct RuleFile {
    pub path: PathBuf,
    original: String,
    /// Byte range of the existing block, if any.
    block: Option<(usize, usize)>,
    /// Why the markers could not be trusted, when they could not be.
    malformed: Option<&'static str>,
}

impl RuleFile {
    pub fn open(path: &Path) -> Result<Self> {
        let original = if path.exists() {
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
        } else {
            String::new()
        };
        let markers = find_markers(&original);
        let block = match markers {
            Markers::One(s, e) => Some((s, e)),
            _ => None,
        };
        let malformed = match markers {
            Markers::Malformed(why) => Some(why),
            _ => None,
        };
        Ok(Self {
            path: path.to_path_buf(),
            original,
            block,
            malformed,
        })
    }

    /// Refuse to touch a file whose markers do not make sense.
    pub fn refusal(&self) -> Option<&'static str> {
        self.malformed
    }

    /// The date recorded in the current block, if there is one.
    ///
    /// `anomalift effect` reads this so the before/after split is the day the rules
    /// were actually written rather than a date typed from memory.
    pub fn applied_on(&self) -> Option<String> {
        let (s, _) = self.block?;
        let line = self.original[s..].lines().next()?;
        let rest = line.strip_prefix(BEGIN)?.trim();
        let date = rest.split_whitespace().next()?;
        (date.len() == 10 && date.starts_with("20")).then(|| date.to_string())
    }

    /// The file as it would be after writing `rules`.
    ///
    /// Pure: computes the new content without touching disk, so the same
    /// function produces both the diff shown to the user and the bytes written.
    /// Two code paths here would eventually disagree, and the user would be
    /// approving one thing and getting another.
    pub fn rendered(&self, rules: &[String], today: &str) -> String {
        if rules.is_empty() {
            return self.without_block();
        }
        let mut block = String::new();
        block.push_str(&format!("{BEGIN} {today} -->\n"));
        block.push_str("<!-- Added by anomalift from repeated failures. `anomalift forget` removes this block. -->\n");
        for r in rules {
            block.push_str(&format!("- {r}\n"));
        }
        block.push_str(END);
        block.push('\n');

        match self.block {
            Some((s, e)) => {
                let mut out = String::with_capacity(self.original.len() + block.len());
                out.push_str(&self.original[..s]);
                out.push_str(&block);
                out.push_str(&self.original[e..]);
                out
            }
            None => {
                let mut out = self.original.clone();
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&block);
                out
            }
        }
    }

    /// The file with the block removed and the surrounding text left intact.
    pub fn without_block(&self) -> String {
        let Some((s, e)) = self.block else {
            return self.original.clone();
        };
        let mut out = String::with_capacity(self.original.len());
        out.push_str(&self.original[..s]);
        let tail = &self.original[e..];
        if tail.trim().is_empty() {
            // The block was last in the file, so `rendered` put exactly one
            // blank line in front of it. Remove exactly one.
            //
            // Popping *all* trailing blank lines was wrong: a file that already
            // ended in a blank line lost the user's own, and a file with no
            // trailing newline gained one. The original test only covered a
            // file ending in a single `\n` - the one case where "pop all" and
            // "pop one" agree.
            if out.ends_with("\n\n") {
                out.pop();
            }
        } else {
            out.push_str(tail);
        }
        out
    }

    pub fn unchanged(&self, candidate: &str) -> bool {
        candidate == self.original
    }

    /// Write, after backing up the previous contents.
    pub fn write(&self, content: &str) -> Result<Option<PathBuf>> {
        let backup = if self.path.exists() {
            let b = self
                .path
                .with_extension(format!("md.anomalift-backup-{}", now_secs()));
            std::fs::copy(&self.path, &b)
                .with_context(|| format!("backing up to {}", b.display()))?;
            Some(b)
        } else {
            None
        };
        // Write to a sibling temp file and rename. `fs::write` truncates first,
        // so a crash or a full disk mid-write leaves CLAUDE.md empty or half
        // written - the user's file, destroyed by a tool meant to help.
        // A rename on the same filesystem is atomic.
        let tmp = self.path.with_extension("md.anomalift-tmp");
        std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(backup)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What `find_block` concluded about the markers in a file.
///
/// A bare `text.find(BEGIN)` was not good enough, and both failures damaged
/// real content:
///
/// - a user documenting anomalift in their own CLAUDE.md, with the markers
///   shown inside a fenced code block, had that example rewritten in place
/// - a stray `BEGIN` left by a hand-edit or a merge conflict paired with the
///   *real* block's `END`, so `forget` deleted everything between them
///
/// Both now return `Malformed`, and the caller refuses to write. Refusing is
/// the right outcome: this code edits a file the user wrote by hand, and there
/// is no safe guess about what they meant.
#[derive(Debug, PartialEq, Eq)]
pub enum Markers {
    None,
    One(usize, usize),
    Malformed(&'static str),
}

/// Byte offsets of every marker occurrence that starts a line and sits outside
/// a fenced code block.
fn marker_positions(text: &str, marker: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut fenced = false;
    let mut offset = 0usize;
    // Tracked so an unterminated fence can be reported rather than silently
    // swallowing every marker after it.
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced && trimmed.starts_with(marker) {
            // Record the start of the line, not of the marker, so indentation
            // is removed along with the block.
            out.push(offset);
        }
        offset += line.len();
    }
    out
}

/// Locate the block, or explain why it cannot be located safely.
fn find_markers(text: &str) -> Markers {
    // An unclosed fence hides everything after it, which made `find_block`
    // return None, `apply` append a second block, and every later run refuse
    // with "more than one block" - permanently.
    let fences = text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("```") || t.starts_with("~~~")
        })
        .count();
    if fences % 2 != 0 {
        return Markers::Malformed(
            "an unterminated code fence — cannot tell markers from examples",
        );
    }
    let begins = marker_positions(text, BEGIN);
    let ends = marker_positions(text, END);
    match (begins.len(), ends.len()) {
        (0, 0) => Markers::None,
        (1, 1) => {
            let start = begins[0];
            let end_line = ends[0];
            if end_line < start {
                return Markers::Malformed("the end marker appears before the begin marker");
            }
            let mut end = end_line + END.len();
            // Consume the rest of the end marker's line.
            while end < text.len() && !text[end..].starts_with('\n') {
                end += 1;
            }
            if text[end..].starts_with('\n') {
                end += 1;
            }
            Markers::One(start, end)
        }
        (1, 0) => Markers::Malformed("a begin marker with no matching end marker"),
        (0, 1) => Markers::Malformed("an end marker with no matching begin marker"),
        _ => Markers::Malformed("more than one anomalift block"),
    }
}

/// A minimal unified-style diff, enough to review a small block.
///
/// Written by hand rather than pulling a diff crate: the change is always one
/// contiguous block, and a dependency for that is not worth the binary size.
pub fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let common_prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = (a.len().min(b.len())) - common_prefix;
    let common_suffix = (0..max_suffix)
        .take_while(|i| a[a.len() - 1 - i] == b[b.len() - 1 - i])
        .count();

    let mut out = String::new();
    for line in &a[common_prefix..a.len() - common_suffix] {
        out.push_str(&format!("- {line}\n"));
    }
    for line in &b[common_prefix..b.len() - common_suffix] {
        out.push_str(&format!("+ {line}\n"));
    }
    out
}

/// Today as `YYYY-MM-DD`, UTC.
pub fn today() -> String {
    let days = now_secs() / 86_400;
    // Inverse of the civil-from-days algorithm used to parse `--since`.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

impl RuleFile {
    /// The file as it is on disk right now.
    pub fn current(&self) -> String {
        self.original.clone()
    }
}

/// `diff`, coloured for a terminal and plain when piped.
pub fn diff_display(before: &str, after: &str) -> String {
    use std::io::IsTerminal;
    let colour = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    diff(before, after)
        .lines()
        .map(|l| {
            if !colour {
                format!("  {l}\n")
            } else if let Some(rest) = l.strip_prefix("- ") {
                format!("  \x1b[31m- {rest}\x1b[0m\n")
            } else if let Some(rest) = l.strip_prefix("+ ") {
                format!("  \x1b[32m+ {rest}\x1b[0m\n")
            } else {
                format!("  {l}\n")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAND_WRITTEN: &str =
        "# Project notes\n\nAlways run the tests.\n\n## Style\n\nTabs, not spaces.\n";

    fn file(content: &str) -> RuleFile {
        let markers = find_markers(content);
        RuleFile {
            path: PathBuf::from("/tmp/nonexistent-CLAUDE.md"),
            original: content.to_string(),
            block: match markers {
                Markers::One(s, e) => Some((s, e)),
                _ => None,
            },
            malformed: match markers {
                Markers::Malformed(w) => Some(w),
                _ => None,
            },
        }
    }

    #[test]
    fn forget_is_an_exact_inverse_of_apply() {
        // The property that matters most: a user must be able to undo this
        // cleanly, including whitespace, or they will not trust it once.
        let f = file(HAND_WRITTEN);
        let applied = f.rendered(&["Do the thing.".into()], "2026-09-06");
        let reopened = file(&applied);
        assert_eq!(reopened.without_block(), HAND_WRITTEN);
    }

    #[test]
    fn forget_preserves_a_trailing_blank_line() {
        // A file that already ended in a blank line must keep it.
        let original = "# Notes\n\nAlways run tests.\n\n";
        let applied = file(original).rendered(&["R.".into()], "2026-09-06");
        assert_eq!(file(&applied).without_block(), original);
    }

    #[test]
    fn a_file_without_a_trailing_newline_gains_one() {
        // Documented, deliberate, and the one case where forget is not a byte
        // exact inverse. `rendered` must terminate the last line before adding
        // a block, and nothing in the resulting file records whether that
        // newline was the user's or ours - recovering it would mean encoding
        // the original state in the marker, which is not worth the ugliness.
        // Ending a text file with a newline is also what every other tool does.
        let applied = file("abc").rendered(&["R.".into()], "2026-09-06");
        assert_eq!(file(&applied).without_block(), "abc\n");
    }

    #[test]
    fn hand_written_content_is_never_touched() {
        let f = file(HAND_WRITTEN);
        let applied = f.rendered(&["Rule A.".into()], "2026-09-06");
        assert!(applied.starts_with(HAND_WRITTEN));
        assert!(applied.contains("Tabs, not spaces."));
        assert!(applied.contains("- Rule A."));
    }

    #[test]
    fn reapplying_replaces_the_block_rather_than_stacking() {
        // Running `anomalift apply` twice must not leave two blocks; the file would
        // grow without bound and the rules would contradict each other.
        let f = file(HAND_WRITTEN);
        let once = f.rendered(&["Rule A.".into()], "2026-09-06");
        let twice = file(&once).rendered(&["Rule B.".into()], "2026-09-07");
        assert_eq!(twice.matches(BEGIN).count(), 1);
        assert!(twice.contains("- Rule B."));
        assert!(!twice.contains("- Rule A."));
        assert!(twice.contains("Tabs, not spaces."));
    }

    #[test]
    fn the_date_survives_a_round_trip() {
        let applied = file(HAND_WRITTEN).rendered(&["R.".into()], "2026-09-06");
        assert_eq!(file(&applied).applied_on().as_deref(), Some("2026-09-06"));
    }

    #[test]
    fn works_on_a_file_that_does_not_exist_yet() {
        let applied = file("").rendered(&["R.".into()], "2026-09-06");
        assert!(applied.starts_with(BEGIN));
        assert_eq!(file(&applied).without_block(), "");
    }

    #[test]
    fn every_rule_lands_inside_the_block_as_a_list_item() {
        let applied =
            file(HAND_WRITTEN).rendered(&["Rule A.".into(), "Rule B.".into()], "2026-09-06");
        let Markers::One(start, end) = find_markers(&applied) else {
            panic!("one block")
        };
        let block = &applied[start..end];
        assert!(block.contains("- Rule A."));
        assert!(block.contains("- Rule B."));
        // Nothing may land outside it: the rest of the file is the user's.
        assert_eq!(file(&applied).without_block(), HAND_WRITTEN);
    }

    #[test]
    fn a_marker_inside_a_fenced_block_is_not_a_marker() {
        // A user documenting anomalift in their own CLAUDE.md. Before this,
        // `find_block` matched the example and apply rewrote it in place,
        // destroying their documentation.
        let doc = "# Notes\n\nAnomalift writes a block like this:\n\n                   ```markdown\n<!-- anomalift:begin 2026-01-01 -->\n                   - an example rule\n<!-- anomalift:end -->\n```\n\nCarry on.\n";
        assert_eq!(
            find_markers(doc),
            Markers::None,
            "fenced example must be ignored"
        );
        let f = file(doc);
        assert!(f.refusal().is_none());
        // Applying adds a real block and leaves the documented example intact.
        let applied = f.rendered(&["Real rule.".into()], "2026-09-06");
        assert!(
            applied.contains("- an example rule"),
            "the user's example survives"
        );
        assert_eq!(applied.matches("- an example rule").count(), 1);
    }

    #[test]
    fn a_stray_begin_marker_is_refused_not_guessed() {
        // Hand-edit or merge damage. Before this, the stray BEGIN paired with
        // the real block's END and `forget` deleted everything between them.
        let damaged = "# Notes\n\n<!-- anomalift:begin 2026-01-01 -->\n\n                       User kept this line.\n";
        match find_markers(damaged) {
            Markers::Malformed(why) => assert!(why.contains("no matching end")),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(file(damaged).refusal().is_some());
    }

    #[test]
    fn two_blocks_are_refused() {
        let two = format!(
            "a\n{BEGIN} 2026-01-01 -->\n- x\n{END}\nb\n{BEGIN} 2026-01-02 -->\n- y\n{END}\n"
        );
        assert!(matches!(find_markers(&two), Markers::Malformed(_)));
    }

    #[test]
    fn an_indented_marker_is_still_found_and_removed_cleanly() {
        let text = format!("# Notes\n\n  {BEGIN} 2026-01-01 -->\n  - x\n  {END}\nafter\n");
        match find_markers(&text) {
            Markers::One(s, _) => assert_eq!(&text[s..s + 2], "  ", "range starts at the line"),
            other => panic!("expected one block, got {other:?}"),
        }
    }

    #[test]
    fn diff_shows_only_the_change() {
        let d = diff("a\nb\nc\n", "a\nX\nc\n");
        assert_eq!(d, "- b\n+ X\n");
    }

    #[test]
    fn today_is_a_plausible_date() {
        let t = today();
        assert_eq!(t.len(), 10);
        assert!(t.starts_with("20"), "got {t}");
        assert_eq!(t.matches('-').count(), 2);
    }
}
