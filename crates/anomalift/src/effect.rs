//! Did the rule actually work?
//!
//! This is the only part of the tool that produces evidence rather than
//! description. Everything else says "this failure recurred"; this says whether
//! writing it down changed anything.
//!
//! **Rates, not counts.** A raw before/after count is confounded by usage: if a
//! rule is added for some command and that command is then never used again,
//! failures drop to zero and the rule looks like a cure. So every pattern gets a
//! denominator - the number of times the agent *attempted* the thing that could
//! have failed - and the comparison is failures per attempt.
//!
//! **"No evidence" is a first-class verdict.** When there are no attempts after
//! the cutoff, the correct answer is that nothing was learned, and it is printed
//! as loudly as a success. Without this, every abandoned tool reads as a win and
//! the tool starts flattering itself.
//!
//! **Token saving is derived from a measurement, not assumed.** The marginal
//! cost of a failure is the output that produced the doomed call, the error
//! text, and the output of the turn that reacted to it. Cached context is
//! excluded - it would have been re-read whether or not the call failed. The
//! constant this yields is a single-history median and is treated as a
//! placeholder; see [`TOKENS_PER_FAILURE`].

use std::collections::HashMap;

use crate::model::{Session, Step};
use crate::signature::signature;

/// Median marginal tokens per failure.
///
/// **A placeholder, not a calibrated constant.** It comes from one machine's
/// history - a few hundred failures, counting the output that produced the
/// doomed call, the error text, and the output of the turn that reacted to it.
/// It is multiplied into every `tokens_saved` figure this tool prints, so a
/// user on a different codebase gets a saving derived from a session
/// distribution that is not theirs. Treat it as an order of magnitude; it
/// should become configurable, and the terminal output says as much wherever
/// the figure appears.
///
/// Deliberately a median rather than a mean: the distribution has a long tail
/// from a handful of very large Edit failures, and a mean flatters the saving.
/// Conservative by choice - this number is used to argue a rule is worth
/// keeping, so it should understate rather than overstate.
pub const TOKENS_PER_FAILURE: u64 = 455;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Rate fell, and the fall is unlikely to be chance.
    Works,
    /// Rate fell, but not enough to distinguish from noise at this sample size.
    Unproven,
    /// The agent never attempted it again, so the rule was never tested.
    NoEvidence,
    /// Rate did not fall, or rose.
    Failed,
}

pub struct Effect {
    pub signature: String,
    pub tool: String,
    pub before_failures: usize,
    pub before_attempts: usize,
    pub after_failures: usize,
    pub after_attempts: usize,
    pub p_value: f64,
    pub verdict: Verdict,
}

impl Effect {
    pub fn before_rate(&self) -> f64 {
        rate(self.before_failures, self.before_attempts)
    }
    pub fn after_rate(&self) -> f64 {
        rate(self.after_failures, self.after_attempts)
    }
    /// Failures avoided, relative to the old rate continuing.
    ///
    /// Expressed against attempts actually made after the cutoff, so a rule
    /// cannot claim credit for work that never happened.
    pub fn failures_avoided(&self) -> f64 {
        let expected = self.before_rate() * self.after_attempts as f64;
        (expected - self.after_failures as f64).max(0.0)
    }
    /// Tokens saved - claimed **only** for a proven effect.
    ///
    /// An `Unproven` verdict means the drop is indistinguishable from chance, so
    /// attributing a saving to it is the tool marking its own homework. On the
    /// first real run, unproven patterns were claiming 25,633 of 27,717 tokens;
    /// every one of those was noise, since no rule had been applied at all.
    pub fn tokens_saved(&self) -> u64 {
        if self.verdict != Verdict::Works {
            return 0;
        }
        (self.failures_avoided() * TOKENS_PER_FAILURE as f64) as u64
    }
}

fn rate(failures: usize, attempts: usize) -> f64 {
    if attempts == 0 {
        0.0
    } else {
        failures as f64 / attempts as f64
    }
}

/// What the agent was *trying* to do when a failure occurred.
///
/// For `Bash` the leading words of the command identify the operation, so
/// `git diff --name-only ...` and `git diff -s ...` share the key `git diff`
/// and a `git commit` is correctly counted as a different thing. For other tools
/// the tool name is the whole story: every `Read` is an opportunity to hit
/// EISDIR.
/// The first segment of a `&&` chain that is not a `cd`.
///
/// Falls back to the whole command when every segment is a `cd`, so a bare
/// `cd nowhere` still keys on `cd` rather than disappearing.
fn first_operation(cmd: &str) -> &str {
    let mut last = cmd.trim();
    for segment in cmd.split("&&") {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        last = segment;
        let first_word = segment.split_whitespace().next().unwrap_or("");
        if first_word != "cd" {
            return segment;
        }
    }
    last
}

pub fn opportunity_key(tool: &str, target: Option<&str>) -> String {
    if tool != "Bash" {
        return tool.to_string();
    }
    let Some(cmd) = target else {
        return tool.to_string();
    };
    // A compound command is keyed on the first thing that actually runs, after
    // skipping `cd` prefixes, which are navigation rather than an operation.
    //
    // Taking the *last* segment instead - which this did - attributes the
    // failure of `npm run build && npm test` to `npm test`, a command that
    // never ran, and counts it against that command's denominator on both
    // sides of the comparison. With `&&`, every later segment is conditional on
    // the earlier ones succeeding, so the first is the one that was certainly
    // attempted.
    let cmd = first_operation(cmd);
    // Skip leading `NAME=VALUE` assignments the way a shell does. Without this
    // the *value* became the first word of the key, so `API_KEY=sk-live-...
    // cargo run` keyed on the secret - and the redacted report printed it
    // verbatim under the words "safe to send as-is". It also fragmented
    // denominators: every distinct value produced a distinct opportunity key.
    let words: Vec<&str> = cmd
        .split_whitespace()
        .skip_while(|w| {
            let Some((name, _)) = w.split_once('=') else {
                return false;
            };
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        })
        .filter(|w| !w.starts_with('-'))
        .take(2)
        .collect();
    if words.is_empty() {
        tool.to_string()
    } else {
        format!("Bash:{}", words.join(" "))
    }
}

fn attempts_key(step: &Step) -> String {
    opportunity_key(&step.tool, step.target.as_deref())
}

/// Measure every recurring pattern against a cutoff time.
///
/// `cutoff` is a unix timestamp: sessions at or after it are "after".
pub fn measure(sessions: &[Session], cutoff: u64) -> Vec<Effect> {
    // signature -> opportunity keys that produced it. A pattern can arise from
    // more than one command shape, and all of them count as attempts.
    let mut keys_for: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut tool_for: HashMap<String, String> = HashMap::new();

    for s in sessions {
        for step in s.failures() {
            let Some(err) = step.error.as_deref() else {
                continue;
            };
            let Some(sig) = signature(&step.tool, err) else {
                continue;
            };
            keys_for
                .entry(sig.clone())
                .or_default()
                .insert(attempts_key(step));
            tool_for.entry(sig).or_insert_with(|| step.tool.clone());
        }
    }

    let mut out = Vec::new();
    for (sig, keys) in keys_for {
        let (mut bf, mut ba, mut af, mut aa) = (0usize, 0usize, 0usize, 0usize);
        for s in sessions {
            let after = s.mtime >= cutoff;
            for step in s.steps() {
                if !keys.contains(&attempts_key(step)) {
                    continue;
                }
                // An attempt is any call of that shape; a failure is one that
                // produced this particular signature.
                let is_this_failure = step.failed
                    && step
                        .error
                        .as_deref()
                        .and_then(|e| signature(&step.tool, e))
                        .is_some_and(|s| s == sig);
                if after {
                    aa += 1;
                    if is_this_failure {
                        af += 1;
                    }
                } else {
                    ba += 1;
                    if is_this_failure {
                        bf += 1;
                    }
                }
            }
        }

        let p = fisher_exact(bf, ba.saturating_sub(bf), af, aa.saturating_sub(af));
        out.push(Effect {
            tool: tool_for.get(&sig).cloned().unwrap_or_default(),
            signature: sig,
            before_failures: bf,
            before_attempts: ba,
            after_failures: af,
            after_attempts: aa,
            p_value: p,
            // Assigned below, once the number of simultaneous tests is known.
            verdict: Verdict::NoEvidence,
        });
    }
    assign_verdicts(&mut out);
    out.sort_by_key(|e| std::cmp::Reverse(e.before_failures));
    out
}

/// Decide each verdict, correcting for the number of tests run at once.
///
/// Hundreds of patterns are tested in a single run, so an uncorrected 0.05
/// threshold guarantees false positives. This is not hypothetical: a run over
/// a few hundred patterns produced a "significant" result at p=0.047 while no
/// rule had been applied at all, so the effect it found could not have existed.
///
/// Holm-Bonferroni rather than plain Bonferroni: it controls the same
/// family-wise error rate while being uniformly less conservative, which matters
/// when a genuine effect has to clear the bar on a few dozen observations.
fn assign_verdicts(effects: &mut [Effect]) {
    // Only patterns with attempts on both sides are testable at all; the rest
    // must not inflate the correction denominator.
    let mut testable: Vec<usize> = (0..effects.len())
        .filter(|&i| effects[i].after_attempts > 0 && effects[i].before_attempts > 0)
        .collect();
    testable.sort_by(|&a, &b| {
        effects[a]
            .p_value
            .partial_cmp(&effects[b].p_value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // `m` counts every testable hypothesis, including those that got worse.
    // Dropping them from the denominator would make the correction weaker than
    // the number of tests actually performed.
    let m = testable.len();

    // Direction first. A pattern whose rate rose is Failed regardless of its
    // p-value, and it must not occupy a rank in the Holm chain.
    //
    // It did, before this fix, and the chain leaked: a worsening pattern at
    // p=0.0471 took rank 0 via `continue` without stopping the chain, so an
    // improving pattern at p=0.0484 was then tested against 0.05/(2-1) = 0.05
    // and printed a green WORKS with token savings. Strict Holm stops at the
    // smallest p-value: 0.0471 > 0.05/2, so nothing should have been rejected.
    let mut improving: Vec<usize> = Vec::new();
    for &i in &testable {
        let e = &effects[i];
        if rate(e.after_failures, e.after_attempts) >= rate(e.before_failures, e.before_attempts) {
            effects[i].verdict = Verdict::Failed;
        } else {
            improving.push(i);
        }
    }

    // Holm over the improving hypotheses only, in ascending p-value. Once one
    // fails its threshold every later one fails too, however small its own p.
    let mut still_rejecting = true;
    for (rank, &i) in improving.iter().enumerate() {
        let threshold = 0.05 / (m - rank) as f64;
        if still_rejecting && effects[i].p_value <= threshold {
            effects[i].verdict = Verdict::Works;
        } else {
            still_rejecting = false;
            effects[i].verdict = Verdict::Unproven;
        }
    }
}

/// Two-sided Fisher's exact test on a 2x2 table.
///
/// Exact rather than chi-squared because the counts here are small - a pattern
/// might be 6 failures in 14 attempts - and the chi-squared approximation is not
/// trustworthy at that size. Computed in log space; the factorials overflow
/// f64 well before the session counts do.
pub fn fisher_exact(a: usize, b: usize, c: usize, d: usize) -> f64 {
    let n = a + b + c + d;
    if n == 0 {
        return 1.0;
    }
    let lf = LnFact::new(n);
    let observed = hyper_logp(&lf, a, b, c, d);
    let row1 = a + b;
    let row2 = c + d;
    let col1 = a + c;
    // x is the top-left cell. It cannot exceed either margin, and it cannot be so
    // small that the bottom-left cell overflows row 2 - hence `col1 - row2`, not
    // `col1 - b`. Getting this wrong silently narrows the enumeration to a couple
    // of tables and returns p = 1 for everything.
    let lo = col1.saturating_sub(row2);
    let hi = row1.min(col1);

    let mut total = 0.0;
    let mut extreme = 0.0;
    for x in lo..=hi {
        let (aa, bb) = (x, row1 - x);
        let (cc, dd) = (col1 - x, row2 - (col1 - x));
        let lp = hyper_logp(&lf, aa, bb, cc, dd);
        let p = lp.exp();
        total += p;
        // 1e-7 guards against a table with the same probability being excluded
        // by floating-point noise, which would understate the p-value.
        if lp <= observed + 1e-7 {
            extreme += p;
        }
    }
    if total <= 0.0 {
        1.0
    } else {
        (extreme / total).min(1.0)
    }
}

fn hyper_logp(t: &LnFact, a: usize, b: usize, c: usize, d: usize) -> f64 {
    t.at(a + b) + t.at(c + d) + t.at(a + c) + t.at(b + d)
        - t.at(a)
        - t.at(b)
        - t.at(c)
        - t.at(d)
        - t.at(a + b + c + d)
}

/// Cumulative log-factorials, built once per test.
///
/// The naive version re-summed from 1 on every call, and the enumeration calls
/// it five times per candidate table - quadratic in the table total. Measured:
/// 4.8ms at n=2,000 but **5.1s at n=100,000**, and a `Read` opportunity key's
/// denominator is every Read call in the window, so a year of transcripts
/// reaches that easily. A user would report it as "it froze".
struct LnFact(Vec<f64>);

impl LnFact {
    fn new(n: usize) -> Self {
        let mut table = Vec::with_capacity(n + 1);
        let mut acc = 0.0;
        table.push(0.0);
        for i in 1..=n {
            acc += (i as f64).ln();
            table.push(acc);
        }
        Self(table)
    }
    fn at(&self, n: usize) -> f64 {
        self.0[n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Query;

    fn step(tool: &str, target: &str, err: Option<&str>) -> Step {
        Step {
            idx: 0,
            tool: tool.into(),
            target: Some(target.into()),
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

    const GIT_ERR: &str = "fatal: options '--name-only' and '-s' cannot be used together";

    #[test]
    fn abandoning_a_tool_is_not_a_cure() {
        // The confounder this module exists for: failures go to zero because the
        // agent stopped using the tool, not because a rule worked.
        let before = session(
            1,
            vec![step("Bash", "git diff --name-only -s", Some(GIT_ERR)); 4],
        );
        let after = session(10, vec![step("Read", "/tmp/x", None)]);
        let effects = measure(&[before, after], 5);
        let e = &effects[0];
        assert_eq!(e.after_attempts, 0);
        assert_eq!(e.verdict, Verdict::NoEvidence);
        // And it must claim nothing.
        assert_eq!(e.tokens_saved(), 0);
    }

    #[test]
    fn a_real_fix_is_detected() {
        let mut before_steps = vec![step("Bash", "git diff --name-only -s", Some(GIT_ERR)); 12];
        before_steps.extend(vec![step("Bash", "git diff HEAD", None); 3]);
        let after_steps = vec![step("Bash", "git diff HEAD", None); 15];
        let effects = measure(&[session(1, before_steps), session(10, after_steps)], 5);
        let e = &effects[0];
        assert_eq!(e.before_failures, 12);
        assert_eq!(e.after_failures, 0);
        assert!(e.after_attempts > 0);
        assert_eq!(e.verdict, Verdict::Works);
        assert!(e.tokens_saved() > 0);
    }

    #[test]
    fn same_rate_is_not_an_improvement() {
        let s = |t| {
            session(
                t,
                vec![
                    step("Bash", "git diff --name-only -s", Some(GIT_ERR)),
                    step("Bash", "git diff HEAD", None),
                ],
            )
        };
        let effects = measure(&[s(1), s(10)], 5);
        assert_eq!(effects[0].verdict, Verdict::Failed);
    }

    #[test]
    fn environment_assignments_are_not_the_command() {
        // A credential must never become the key. This reached a real report.
        assert_eq!(
            opportunity_key("Bash", Some("API_KEY=sk-live-4242abcd cargo run")),
            "Bash:cargo run"
        );
        assert_eq!(
            opportunity_key("Bash", Some("AWS_PROFILE=acme-prod aws s3 ls")),
            "Bash:aws s3"
        );
        // Two assignments, and one that only looks like one.
        assert_eq!(
            opportunity_key("Bash", Some("A=1 B=2 npm test")),
            "Bash:npm test"
        );
        assert_eq!(
            opportunity_key("Bash", Some("git log --grep=fix")),
            "Bash:git log"
        );
    }

    #[test]
    fn opportunity_key_separates_git_subcommands() {
        assert_eq!(
            opportunity_key("Bash", Some("git diff --name-only")),
            "Bash:git diff"
        );
        assert_eq!(
            opportunity_key("Bash", Some("git commit -m x")),
            "Bash:git commit"
        );
        // A leading `cd` must not become the key, or every command in a repo
        // would share one denominator.
        assert_eq!(
            opportunity_key("Bash", Some("cd /tmp && git diff HEAD")),
            "Bash:git diff"
        );
        // Non-Bash tools are their own opportunity class.
        assert_eq!(opportunity_key("Read", Some("/a/b")), "Read");
    }

    #[test]
    fn a_compound_command_is_keyed_on_what_actually_ran() {
        // `&&` makes every later segment conditional, so the failure belongs to
        // the first segment. Keying on the last one charged it to a command
        // that never executed - and then counted that command's successful runs
        // as its denominator, on both sides of the comparison.
        assert_eq!(
            opportunity_key("Bash", Some("npm run build && npm test")),
            "Bash:npm run"
        );
        // Several `cd`s in front are still just navigation.
        assert_eq!(
            opportunity_key("Bash", Some("cd /a && cd b && cargo test")),
            "Bash:cargo test"
        );
        // A command that is nothing but `cd` keys on `cd`, not on nothing.
        assert_eq!(
            opportunity_key("Bash", Some("cd /nowhere")),
            "Bash:cd /nowhere"
        );
    }

    #[test]
    fn a_worsening_pattern_cannot_unblock_the_holm_chain() {
        // The counterexample from review, executed. A got worse (p=0.0471, the smallest
        // p-value); B improved (p=0.0484). Strict Holm stops at the smallest
        // p-value - 0.0471 > 0.05/2 - so nothing may be rejected. Before the
        // fix, A consumed rank 0 via `continue` without stopping the chain and
        // B was tested against 0.05/1, printing WORKS with token savings.
        let mut effects = vec![
            Effect {
                signature: "A".into(),
                tool: "Bash".into(),
                before_failures: 0,
                before_attempts: 20,
                after_failures: 5,
                after_attempts: 20,
                p_value: fisher_exact(0, 20, 5, 15),
                verdict: Verdict::NoEvidence,
            },
            Effect {
                signature: "B".into(),
                tool: "Bash".into(),
                before_failures: 11,
                before_attempts: 20,
                after_failures: 4,
                after_attempts: 20,
                p_value: fisher_exact(11, 9, 4, 16),
                verdict: Verdict::NoEvidence,
            },
        ];
        assign_verdicts(&mut effects);
        assert_eq!(effects[0].verdict, Verdict::Failed, "A got worse");
        assert_eq!(
            effects[1].verdict,
            Verdict::Unproven,
            "B must not be rejected: the smallest p-value did not clear 0.05/2"
        );
        assert_eq!(effects[1].tokens_saved(), 0, "and it must claim nothing");
    }

    #[test]
    fn fisher_matches_known_values() {
        // Tea-tasting table: 3,1,1,3 has a two-sided p of ~0.4857.
        let p = fisher_exact(3, 1, 1, 3);
        assert!((p - 0.4857).abs() < 0.001, "got {p}");
        // No difference at all must not be significant.
        assert!(fisher_exact(5, 5, 5, 5) > 0.9);
        // A clean separation must be.
        assert!(fisher_exact(12, 3, 0, 15) < 0.001);
    }
}

/// Serializable view for `--json`.
///
/// The verdict is emitted as a string so consumers never re-derive it from the
/// counts and reach a different conclusion than the terminal output did.
#[derive(serde::Serialize)]
pub struct Row {
    pub signature: String,
    pub tool: String,
    pub before_failures: usize,
    pub before_attempts: usize,
    pub after_failures: usize,
    pub after_attempts: usize,
    pub before_rate: f64,
    pub after_rate: f64,
    pub p_value: f64,
    pub verdict: &'static str,
    pub failures_avoided: f64,
    pub tokens_saved: u64,
}

/// The machine-readable name of a verdict.
///
/// One definition, used by `--json` and by the history written to the store, so
/// a verdict cannot be spelled two ways in two files that are later compared.
pub fn verdict_word(v: &Verdict) -> &'static str {
    match v {
        Verdict::Works => "works",
        Verdict::Unproven => "unproven",
        Verdict::NoEvidence => "no_evidence",
        Verdict::Failed => "failed",
    }
}

pub fn as_rows(effects: &[Effect]) -> Vec<Row> {
    effects
        .iter()
        .map(|e| Row {
            signature: e.signature.clone(),
            tool: e.tool.clone(),
            before_failures: e.before_failures,
            before_attempts: e.before_attempts,
            after_failures: e.after_failures,
            after_attempts: e.after_attempts,
            before_rate: e.before_rate(),
            after_rate: e.after_rate(),
            p_value: e.p_value,
            verdict: verdict_word(&e.verdict),
            // Gated exactly like tokens_saved. Emitting a positive figure for
            // an unproven row lets a JSON consumer re-derive the savings the
            // terminal deliberately suppresses.
            failures_avoided: if e.verdict == Verdict::Works {
                e.failures_avoided()
            } else {
                0.0
            },
            tokens_saved: e.tokens_saved(),
        })
        .collect()
}

/// Baseline statistics for one signature, as of the sessions given.
///
/// Split out from `measure` so `anomalift apply` can freeze these numbers at the moment
/// a rule is written. Computing them later, from a different session window,
/// silently changes the denominator a verdict rests on.
pub struct BaselineStats {
    pub failures: usize,
    pub attempts: usize,
    pub keys: std::collections::BTreeSet<String>,
    /// Sessions these numbers came from, so they can be excluded later.
    pub sessions: Vec<String>,
}

pub fn baseline_for(sessions: &[Session], sig: &str) -> BaselineStats {
    let mut keys: std::collections::BTreeSet<String> = Default::default();
    for s in sessions {
        for step in s.failures() {
            let Some(err) = step.error.as_deref() else {
                continue;
            };
            if signature(&step.tool, err).as_deref() == Some(sig) {
                keys.insert(attempts_key(step));
            }
        }
    }
    let (mut failures, mut attempts) = (0usize, 0usize);
    for s in sessions {
        for step in s.steps() {
            if !keys.contains(&attempts_key(step)) {
                continue;
            }
            attempts += 1;
            if step.failed
                && step
                    .error
                    .as_deref()
                    .and_then(|e| signature(&step.tool, e))
                    .as_deref()
                    == Some(sig)
            {
                failures += 1;
            }
        }
    }
    BaselineStats {
        failures,
        attempts,
        keys,
        sessions: sessions.iter().map(|s| s.id.clone()).collect(),
    }
}

/// Measure stored rules against their **frozen** baselines.
///
/// The difference from `measure` is the whole point of the store: the "before"
/// numbers come from what was true when the rule was written, not from
/// recomputing over whatever sessions are currently in the window. Recomputation
/// made one pattern's baseline read three different values for the same data,
/// depending only on `--sessions`.
pub fn measure_stored(sessions: &[Session], store: &crate::store::Store) -> Vec<Effect> {
    let mut out = Vec::new();
    for rule in &store.rules {
        let (mut af, mut aa) = (0usize, 0usize);
        for s in sessions {
            // Only sessions after the rule was written can test it, and never a
            // session the baseline was computed from - mtime resolution is one
            // second, so the id check is what actually separates the two sides.
            if s.mtime < rule.applied_at || rule.baseline_sessions.contains(&s.id) {
                continue;
            }
            for step in s.steps() {
                if !rule.opportunity_keys.contains(&attempts_key(step)) {
                    continue;
                }
                aa += 1;
                if step.failed
                    && step
                        .error
                        .as_deref()
                        .and_then(|e| signature(&step.tool, e))
                        .as_deref()
                        == Some(rule.signature.as_str())
                {
                    af += 1;
                }
            }
        }
        let bf = rule.baseline.failures;
        let ba = rule.baseline.attempts;
        let p = fisher_exact(bf, ba.saturating_sub(bf), af, aa.saturating_sub(af));
        out.push(Effect {
            signature: rule.signature.clone(),
            tool: rule.tool.clone(),
            before_failures: bf,
            before_attempts: ba,
            after_failures: af,
            after_attempts: aa,
            p_value: p,
            verdict: Verdict::NoEvidence,
        });
    }
    assign_verdicts(&mut out);
    out.sort_by_key(|e| std::cmp::Reverse(e.before_failures));
    out
}
