//! What the tool has learned, stored as data rather than prose.
//!
//! `CLAUDE.md` holds the rule the *agent* reads. This holds what `anomalift` needs to
//! know to judge that rule later, and the two are not the same thing. A line of
//! advice cannot record when it was written, what the failure rate was at the
//! time, or which command shapes count as an attempt.
//!
//! **The reason this exists is a correctness bug, not tidiness.** Before it,
//! `anomalift effect` recomputed the "before" window from whatever sessions happened to
//! be in range, so the same pattern's baseline read a different value for the
//! same data purely according to `--sessions`. A verdict that moves when you change how far back
//! you look is not evidence of anything. The baseline is now **frozen at the
//! moment the rule is applied** and never recomputed.
//!
//! Three things are frozen deliberately:
//!
//! - `baseline` - failures and attempts as they stood when the rule was written
//! - `opportunity_keys` - which command shapes count as an attempt, since these
//!   are derived from observed failures and would otherwise drift as new
//!   failures appear
//! - `applied_at` - the cutoff, so a later measurement cannot quietly move it
//!
//! Measurements accumulate rather than overwrite. A rule that looked unproven at
//! twenty sessions and works at eighty is a different claim from one that has
//! only ever been measured once, and keeping the history makes that visible.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Bumped when the on-disk shape changes incompatibly. An unknown version is
/// refused rather than guessed at: silently misreading a baseline would produce
/// confident, wrong verdicts.
const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub failures: usize,
    pub attempts: usize,
    /// How many sessions the baseline was computed over, for context when a
    /// user asks why a rate looks the way it does.
    pub sessions_scanned: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    pub on: String,
    pub failures: usize,
    pub attempts: usize,
    pub p_value: f64,
    pub verdict: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedRule {
    /// The normalised failure signature. Stable identity across runs.
    pub signature: String,
    pub tool: String,
    /// Command shapes that count as an attempt. Frozen: recomputing these later
    /// would change the denominator underneath a measurement in progress.
    pub opportunity_keys: BTreeSet<String>,
    pub rule: String,
    /// True when this pattern was frozen as a **control**, not acted on.
    ///
    /// Recurring patterns with no known remedy are measured but never written
    /// to CLAUDE.md. They share the selection bias, the agent and the drift of
    /// the ruled patterns, so their movement is what ordinary change looks like
    /// with no rule involved. Without them a drop cannot be attributed: rules
    /// are only written for patterns that cleared a threshold, which selects
    /// the ones noise pushed up, and regression to the mean alone will make
    /// them fall.
    #[serde(default)]
    pub control: bool,
    /// User requests the agent was working on when this failed.
    ///
    /// Frozen with the rule so a later reader can judge whether the advice
    /// suits the situations it actually arises in - a rule that only ever fires
    /// during one kind of task should probably be conditioned on that task
    /// rather than stated unconditionally.
    #[serde(default)]
    pub example_queries: Vec<String>,
    pub applied_on: String,
    pub applied_at: u64,
    /// Session ids the baseline was computed from.
    ///
    /// Excluded from every later "after" window. Splitting on `applied_at`
    /// alone is not enough: session mtimes have one-second resolution, so a
    /// session written in the same second as `apply` - and the session in
    /// flight when the agent runs `apply` itself - lands on both sides of the
    /// table. Its failures then count as baseline *and* as evidence against
    /// the rule, which is how a correct rule shows "no improvement".
    #[serde(default)]
    pub baseline_sessions: Vec<String>,
    pub baseline: Baseline,
    #[serde(default)]
    pub measurements: Vec<Measurement>,
}

impl LearnedRule {
    pub fn latest(&self) -> Option<&Measurement> {
        self.measurements.last()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    pub version: u32,
    #[serde(default)]
    pub rules: Vec<LearnedRule>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: VERSION,
            rules: Vec::new(),
        }
    }
}

impl Store {
    /// Beside the rules file, so what was applied and what is being measured
    /// travel together. A project copied to another machine keeps its history.
    pub fn path_for(rules_file: &Path) -> PathBuf {
        rules_file
            .parent()
            .unwrap_or(Path::new("."))
            .join(".anomalift")
            .join("learned.json")
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let store: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if store.version > VERSION {
            anyhow::bail!(
                "{} was written by a newer anomalift (v{} > v{}); upgrade rather than risk \
                 misreading a frozen baseline",
                path.display(),
                store.version,
                VERSION
            );
        }
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // Pretty-printed on purpose: this is a file a human may need to read to
        // understand why a verdict came out the way it did, and it lands in a
        // repo where a diff should be legible.
        let text = serde_json::to_string_pretty(self)?;
        // Temp + rename, like `rules.rs`. This file holds the frozen baselines;
        // a truncated write loses the only record of what was true when a rule
        // was applied, and that cannot be recomputed from anything.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text + "\n").with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    /// Where the scan cache lives, beside the store.
    ///
    /// `mcp` answers per-tool-call and must be instant; re-reading 192MB of
    /// transcripts on every check would make the agent slower than the failures
    /// it is avoiding. The scan writes this; the server only reads it.
    pub fn cache_path_for(rules_file: &Path) -> PathBuf {
        rules_file
            .parent()
            .unwrap_or(Path::new("."))
            .join(".anomalift")
            .join("patterns.json")
    }

    /// Where live failure capture appends, beside the store.
    ///
    /// A third file rather than a corner of `learned.json`, deliberately. That
    /// file holds frozen baselines and is rewritten wholesale by `anomalift
    /// apply`; a hook firing during a rewrite would either be lost or would
    /// clobber a baseline, and losing a baseline is the one failure this crate
    /// cannot recover from. `patterns.json` is likewise a derived cache the
    /// scan overwrites. Observations are neither - they are append-only
    /// evidence - so they get their own file with its own write discipline.
    ///
    /// `.jsonl`, not `.json`, because the contents genuinely are one JSON
    /// object per line rather than an array. See `hook` for why that format is
    /// the concurrency answer; the extension should not lie about it.
    pub fn observed_path_for(rules_file: &Path) -> PathBuf {
        rules_file
            .parent()
            .unwrap_or(Path::new("."))
            .join(".anomalift")
            .join("observed.jsonl")
    }

    pub fn get(&self, signature: &str) -> Option<&LearnedRule> {
        self.rules.iter().find(|r| r.signature == signature)
    }

    /// Record a newly applied rule.
    ///
    /// Re-applying an existing rule keeps the original baseline and date. The
    /// whole point of freezing is defeated if running `anomalift apply` again resets
    /// the clock - a user who reruns it weekly would never accumulate evidence.
    pub fn record(&mut self, rule: LearnedRule) -> bool {
        if let Some(existing) = self
            .rules
            .iter_mut()
            .find(|r| r.signature == rule.signature)
        {
            // Rule text may improve; the measurement anchors must not move.
            existing.rule = rule.rule;
            false
        } else {
            self.rules.push(rule);
            true
        }
    }

    pub fn add_measurement(&mut self, signature: &str, m: Measurement) {
        if let Some(r) = self.rules.iter_mut().find(|r| r.signature == signature) {
            // Replace a measurement taken the same day rather than accumulating
            // duplicates when a user runs `anomalift effect` repeatedly.
            if r.measurements.last().map(|l| l.on.as_str()) == Some(m.on.as_str()) {
                r.measurements.pop();
            }
            r.measurements.push(m);
        }
    }

    pub fn remove_all(&mut self) -> usize {
        let n = self.rules.len();
        self.rules.clear();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(sig: &str, failures: usize, attempts: usize) -> LearnedRule {
        LearnedRule {
            signature: sig.into(),
            tool: "Bash".into(),
            opportunity_keys: ["Bash:git diff".to_string()].into_iter().collect(),
            rule: "do not".into(),
            control: false,
            example_queries: vec!["fix the retry timeout".into()],
            applied_on: "2026-09-06".into(),
            applied_at: 1_788_600_000,
            baseline_sessions: vec!["s0".into()],
            baseline: Baseline {
                failures,
                attempts,
                sessions_scanned: 120,
            },
            measurements: Vec::new(),
        }
    }

    #[test]
    fn reapplying_does_not_reset_the_baseline() {
        // The bug this whole module exists to prevent: a user who reruns
        // `anomalift apply` weekly would restart the clock every time and never
        // accumulate enough evidence to prove anything.
        let mut s = Store::default();
        assert!(s.record(rule("sig", 19, 160)));
        assert!(!s.record(rule("sig", 0, 5)));
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].baseline.failures, 19);
        assert_eq!(s.rules[0].baseline.attempts, 160);
        assert_eq!(s.rules[0].applied_on, "2026-09-06");
    }

    #[test]
    fn measuring_twice_in_a_day_replaces_rather_than_stacks() {
        let mut s = Store::default();
        s.record(rule("sig", 19, 160));
        for f in [3, 2] {
            s.add_measurement(
                "sig",
                Measurement {
                    on: "2026-09-20".into(),
                    failures: f,
                    attempts: 40,
                    p_value: 0.2,
                    verdict: "unproven".into(),
                },
            );
        }
        assert_eq!(s.rules[0].measurements.len(), 1);
        assert_eq!(s.rules[0].measurements[0].failures, 2);
    }

    #[test]
    fn history_accumulates_across_days() {
        let mut s = Store::default();
        s.record(rule("sig", 19, 160));
        for day in ["2026-09-20", "2026-10-01"] {
            s.add_measurement(
                "sig",
                Measurement {
                    on: day.into(),
                    failures: 1,
                    attempts: 50,
                    p_value: 0.01,
                    verdict: "works".into(),
                },
            );
        }
        assert_eq!(s.rules[0].measurements.len(), 2);
        assert_eq!(s.rules[0].latest().unwrap().on, "2026-10-01");
    }

    #[test]
    fn a_newer_on_disk_version_is_refused() {
        let dir = std::env::temp_dir().join(format!("anomalift-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("learned.json");
        std::fs::write(&p, r#"{"version":99,"rules":[]}"#).unwrap();
        let err = Store::load(&p).unwrap_err().to_string();
        assert!(err.contains("newer anomalift"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("anomalift-store-rt-{}", std::process::id()));
        let p = dir.join("learned.json");
        let mut s = Store::default();
        s.record(rule("sig", 19, 160));
        s.save(&p).unwrap();
        let back = Store::load(&p).unwrap();
        assert_eq!(back.rules.len(), 1);
        assert_eq!(back.rules[0].baseline.attempts, 160);
        assert!(back.rules[0].opportunity_keys.contains("Bash:git diff"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
