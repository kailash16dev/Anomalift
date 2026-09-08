//! The report a beta tester sends back.
//!
//! This is the only thing that ever leaves a user's machine, and it leaves it by
//! hand: the file is written to disk and the user chooses whether to send it.
//! Nothing is uploaded, and the tool makes no network call.
//!
//! **Redaction is allow-list, not deny-list.** Only fields known to be safe are
//! emitted - counts, rates, verdicts, tool names, truncated command shapes. A
//! deny-list would have to anticipate every way private text can appear, and it
//! would be wrong the first time someone's error message contained a client
//! name. This is not a hypothetical: the tool's own `--json` output carries a
//! `queries` field of verbatim ticket titles and task descriptions, and even a
//! normalised signature can contain captured stdout. None of that appears here.
//!
//! **The control arm is the point.** Reporting only ruled patterns would be
//! useless: rules are written for patterns that cleared a threshold, which
//! selects the ones noise pushed up, so they fall again on their own. Patterns
//! that recurred but had no known remedy are frozen as controls and reported
//! beside them. If both fall by the same amount, the rules did nothing, and
//! the report says so in those words.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::effect::{Effect, Verdict};
use crate::store::Store;

/// Subcommands that are safe to print, by program.
///
/// An allow-list of *values*, not of shapes. Two shape heuristics have now
/// failed here, each leaking real content:
///
///   `cat SidePanelPageLayout`  - a source filename; nothing about the word
///                                looked unsafe, so "drop argument-shaped
///                                words" let it through
///   `git my-client-name`       - a branch named after a client; lowercase and
///                                hyphenated, so "keep lowercase subcommands"
///                                let it through
///
/// There is no property that distinguishes `show` from `my-client-name` other
/// than being a known subcommand. So the list is enumerated, and anything not
/// on it is dropped. Missing a legitimate subcommand costs a little context in
/// a report; missing a client name costs someone their job.
const SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "git",
        &[
            "show",
            "log",
            "diff",
            "status",
            "commit",
            "checkout",
            "branch",
            "merge",
            "rebase",
            "fetch",
            "pull",
            "push",
            "stash",
            "add",
            "reset",
            "worktree",
            "cat-file",
            "rev-parse",
            "ls-files",
            "blame",
            "tag",
            "clone",
            "init",
        ],
    ),
    (
        "npm",
        &[
            "install", "run", "test", "ci", "publish", "audit", "ls", "link", "exec",
        ],
    ),
    ("pnpm", &["install", "run", "test", "add", "build", "exec"]),
    ("yarn", &["install", "run", "test", "add", "build"]),
    (
        "cargo",
        &[
            "build", "test", "run", "check", "clippy", "fmt", "add", "publish", "bench", "doc",
            "tree", "install",
        ],
    ),
    (
        "docker",
        &[
            "build", "run", "ps", "exec", "compose", "pull", "push", "logs", "rm",
        ],
    ),
    (
        "kubectl",
        &[
            "get", "describe", "apply", "delete", "logs", "exec", "rollout",
        ],
    ),
    ("go", &["build", "test", "run", "mod", "get", "vet", "fmt"]),
    ("uv", &["run", "sync", "add", "pip", "venv"]),
    ("pip", &["install", "uninstall", "list", "freeze"]),
    ("brew", &["install", "update", "upgrade", "list", "info"]),
    ("gh", &["pr", "issue", "repo", "release", "run", "api"]),
    ("aws", &["s3", "ec2", "logs", "sts"]),
    ("terraform", &["plan", "apply", "init", "destroy", "fmt"]),
    (
        "systemctl",
        &["status", "start", "stop", "restart", "enable"],
    ),
];

/// Programs common enough that naming them cannot identify anyone.
const COMMON_PROGRAMS: &[&str] = &[
    "cat", "ls", "cd", "grep", "rg", "find", "sed", "awk", "echo", "make", "python", "python3",
    "node", "sh", "bash", "curl", "wget", "mkdir", "rm", "cp", "mv", "touch", "head", "tail", "wc",
    "diff", "test", "which", "env",
];

/// Reduce a command shape to something safe to send.
///
/// `git show` survives; `git my-client-name` becomes `git`; `cat AnyFile`
/// becomes `cat`; a path becomes `(command)`. Losing detail is the intent - the
/// shape is context for a maintainer reading counts, not data to analyse.
fn safe_shape(key: &str) -> String {
    let body = key.strip_prefix("Bash:").unwrap_or(key);
    if body == key {
        // Not a Bash key - a bare tool name like `Read`, already safe.
        return key.to_string();
    }
    let mut words = body.split_whitespace();
    let Some(program) = words.next() else {
        return "(command)".to_string();
    };
    if program.contains('/') || program.contains('\\') || program.starts_with('.') {
        // A path, not a program name: `./deploy-acme.sh` names a client.
        return "(command)".to_string();
    }
    // Belt and braces against the environment-assignment leak. `opportunity_key`
    // now strips these, but old frozen keys in an existing store still carry
    // them, and a secret must not reach the report because of an upgrade order.
    if program.contains('=') {
        return "(command)".to_string();
    }
    // An unknown program is very often an internal CLI named after the company
    // that wrote it - `acmectl`, `bigco-deploy`. The known-program list is the
    // only thing that makes a name safe to print.
    if !SUBCOMMANDS.iter().any(|(p, _)| *p == program) && !COMMON_PROGRAMS.contains(&program) {
        return "(command)".to_string();
    }
    match words.next() {
        Some(sub)
            if SUBCOMMANDS
                .iter()
                .any(|(p, subs)| *p == program && subs.contains(&sub)) =>
        {
            format!("{program} {sub}")
        }
        _ => program.to_string(),
    }
}

fn pct(n: usize, d: usize) -> String {
    if d == 0 {
        "—".to_string()
    } else {
        format!("{:.1}%", n as f64 / d as f64 * 100.0)
    }
}

/// Median of a set of before→after rate changes, in percentage points.
fn median_drop(effects: &[&Effect]) -> Option<f64> {
    let mut drops: Vec<f64> = effects
        .iter()
        .filter(|e| e.before_attempts > 0 && e.after_attempts > 0)
        .map(|e| (e.before_rate() - e.after_rate()) * 100.0)
        .collect();
    if drops.is_empty() {
        return None;
    }
    drops.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(drops[drops.len() / 2])
}

fn verdict_word(v: &Verdict) -> &'static str {
    match v {
        Verdict::Works => "works",
        Verdict::Unproven => "unproven",
        Verdict::NoEvidence => "no evidence",
        Verdict::Failed => "no improvement",
    }
}

pub struct Report {
    pub path: PathBuf,
    pub body: String,
    pub ruled: usize,
    pub controls: usize,
}

/// Build the report. Pure - takes what it needs, writes nothing.
pub fn render(store: &Store, effects: &[Effect], sessions: usize, days: u64) -> Report {
    let mut out = String::new();
    let version = env!("CARGO_PKG_VERSION");

    out.push_str("# Anomalift report\n\n");
    out.push_str(&format!(
        "- anomalift `{version}` on `{}`\n- {sessions} sessions scanned, spanning {days} days\n",
        std::env::consts::OS,
    ));
    out.push_str("- generated locally; contains no file paths, queries or error text\n\n");

    // Split the measured effects by whether a rule was actually written.
    let is_control = |sig: &str| store.get(sig).map(|r| r.control).unwrap_or(false);
    let ruled: Vec<&Effect> = effects
        .iter()
        .filter(|e| !is_control(&e.signature))
        .collect();
    let controls: Vec<&Effect> = effects
        .iter()
        .filter(|e| is_control(&e.signature))
        .collect();

    out.push_str("## Did the rules work?\n\n");
    match (median_drop(&ruled), median_drop(&controls)) {
        (Some(r), Some(c)) => {
            out.push_str(&format!(
                "| arm | patterns | median drop |\n|---|---|---|\n\
                 | with a rule | {} | {:+.1} pp |\n| control, no rule | {} | {:+.1} pp |\n\n",
                ruled.len(),
                r,
                controls.len(),
                c
            ));
            out.push_str(&format!("**Difference: {:+.1} pp.** ", r - c));
            // The interpretation is written here, in the artifact, rather than
            // left to whoever reads it later.
            if r - c <= 0.0 {
                out.push_str(
                    "Ruled patterns did **not** fall further than untouched ones, \
                     so this data does not show the rules doing anything.\n\n",
                );
            } else {
                out.push_str(
                    "Ruled patterns fell further than untouched ones. Whether that gap \
                     is real depends on how many patterns and sessions are behind it — \
                     see the counts above.\n\n",
                );
            }
        }
        (Some(r), None) => {
            out.push_str(&format!(
                "{} ruled patterns, median drop {:+.1} pp.\n\n\
                 **No control arm in this data**, so the drop cannot be separated from \
                 ordinary drift. A control needs recurring patterns that had no known fix.\n\n",
                ruled.len(),
                r
            ));
        }
        _ => {
            out.push_str(
                "Not enough measured patterns yet. `effect` needs sessions recorded \
                 *after* the rules were applied — usually two to three weeks of normal use.\n\n",
            );
        }
    }

    let section = |out: &mut String, title: &str, rows: &[&Effect]| {
        out.push_str(&format!("## {title}\n\n"));
        if rows.is_empty() {
            out.push_str("_none_\n\n");
            return;
        }
        out.push_str("| command | before | after | verdict | p |\n|---|---|---|---|---|\n");
        for e in rows {
            let shape = store
                .get(&e.signature)
                .and_then(|r| r.opportunity_keys.iter().next().cloned())
                .map(|k| safe_shape(&k))
                .unwrap_or_else(|| e.tool.clone());
            out.push_str(&format!(
                "| `{}` | {}/{} ({}) | {}/{} ({}) | {} | {:.3} |\n",
                shape,
                e.before_failures,
                e.before_attempts,
                pct(e.before_failures, e.before_attempts),
                e.after_failures,
                e.after_attempts,
                pct(e.after_failures, e.after_attempts),
                verdict_word(&e.verdict),
                e.p_value,
            ));
        }
        out.push('\n');
    };

    section(&mut out, "Patterns with a rule", &ruled);
    section(&mut out, "Control patterns (no rule written)", &controls);

    out.push_str("## What is not in this file\n\n");
    out.push_str(
        "No file paths, no queries or task descriptions, no error text, no repository \
         or project names, no usernames, no machine identifiers. Command shapes are \
         truncated to the program and subcommand.\n",
    );

    Report {
        path: PathBuf::new(),
        body: out,
        ruled: ruled.len(),
        controls: controls.len(),
    }
}

pub fn write(report: &Report, path: &Path) -> Result<()> {
    std::fs::write(path, &report.body).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Baseline, LearnedRule};

    fn rule(sig: &str, key: &str, control: bool) -> LearnedRule {
        LearnedRule {
            signature: sig.into(),
            tool: "Bash".into(),
            opportunity_keys: [key.to_string()].into_iter().collect(),
            rule: if control {
                String::new()
            } else {
                "do not".into()
            },
            control,
            example_queries: vec!["[TICKET-1] internal task description".into()],
            applied_on: "2026-09-06".into(),
            applied_at: 1_788_600_000,
            baseline_sessions: vec!["s1".into()],
            baseline: Baseline {
                failures: 19,
                attempts: 129,
                sessions_scanned: 120,
            },
            measurements: vec![],
        }
    }

    fn effect(sig: &str, bf: usize, ba: usize, af: usize, aa: usize) -> Effect {
        Effect {
            signature: sig.into(),
            tool: "Bash".into(),
            before_failures: bf,
            before_attempts: ba,
            after_failures: af,
            after_attempts: aa,
            p_value: 0.01,
            verdict: Verdict::Works,
        }
    }

    #[test]
    fn never_leaks_private_text() {
        // The whole reason this module exists. The store deliberately holds a
        // ticket title and an absolute path; neither may reach the report.
        let mut store = Store::default();
        store.record(rule(
            "Bash: fatal: boom",
            "Bash:cd /Users/someone/clients/acme",
            false,
        ));
        let r = render(
            &store,
            &[effect("Bash: fatal: boom", 19, 129, 1, 84)],
            120,
            30,
        );

        assert!(!r.body.contains("TICKET-1"), "ticket title leaked");
        assert!(!r.body.contains("/Users/"), "absolute path leaked");
        assert!(!r.body.contains("someone"), "username leaked");
        assert!(!r.body.contains("acme"), "client name leaked");
        assert!(!r.body.contains("fatal: boom"), "raw signature leaked");
    }

    #[test]
    fn shapes_are_truncated_to_program_and_subcommand() {
        assert_eq!(safe_shape("Bash:git show"), "git show");
        assert_eq!(safe_shape("Read"), "Read");
        assert_eq!(safe_shape("Bash:cd /Users/x/client"), "cd");
        assert_eq!(safe_shape("Bash:./secret-script.sh"), "(command)");
    }

    #[test]
    fn a_private_name_shaped_like_a_subcommand_never_survives() {
        // Both of these passed an earlier shape heuristic. A branch or script
        // named after a client is lowercase and hyphenated, exactly like a real
        // subcommand, so only an enumerated list can separate them.
        assert_eq!(safe_shape("Bash:git my-client-name"), "git");
        // A bare `cd` keys on its argument, which is a path into someone's
        // tree. `cd` is a known program, so the guard that matters is the one
        // that drops any second word which is not a known subcommand.
        assert_eq!(safe_shape("Bash:cd packages/internal-thing"), "cd");
        assert_eq!(safe_shape("Bash:docker build-acme"), "docker");
        assert_eq!(safe_shape("Bash:npm run"), "npm run");
        assert_eq!(safe_shape("Bash:gh pr"), "gh pr");
        // An unknown program is likely an internal tool named after a company.
        assert_eq!(safe_shape("Bash:acmectl deploy"), "(command)");
        // And a secret that survived in an old frozen key never prints.
        assert_eq!(safe_shape("Bash:API_KEY=sk-live-4242 cargo"), "(command)");
    }

    #[test]
    fn a_filename_argument_never_survives() {
        // Caught in a real report: `cat SidePanelPageLayout` reached the file,
        // leaking a source filename. Nothing about that word looks unsafe -
        // hence the allow-list.
        assert_eq!(safe_shape("Bash:cat SidePanelPageLayout"), "cat");
        assert_eq!(safe_shape("Bash:node scripts/deploy-acme.js"), "node");
        assert_eq!(safe_shape("Bash:python train_client_model.py"), "python");
        // But a real subcommand is kept, because it carries no private content.
        assert_eq!(safe_shape("Bash:npm install"), "npm install");
        assert_eq!(safe_shape("Bash:cargo test"), "cargo test");
        // Even for an allow-listed program, a CamelCase word is an argument.
        assert_eq!(safe_shape("Bash:git MyPrivateBranch"), "git");
    }

    #[test]
    fn control_arm_is_reported_beside_the_ruled_one() {
        let mut store = Store::default();
        store.record(rule("A", "Bash:git show", false));
        store.record(rule("B", "Bash:npm install", true));
        // Ruled falls a lot; control falls a little.
        let effects = vec![effect("A", 19, 129, 1, 84), effect("B", 20, 100, 15, 90)];
        let r = render(&store, &effects, 120, 30);
        assert_eq!(r.ruled, 1);
        assert_eq!(r.controls, 1);
        assert!(r.body.contains("control, no rule"));
        assert!(r.body.contains("Difference:"));
    }

    #[test]
    fn says_plainly_when_the_rules_did_nothing() {
        // Control falls further than the ruled arm: the honest read is that
        // nothing was demonstrated, and the report must say so unprompted.
        let mut store = Store::default();
        store.record(rule("A", "Bash:git show", false));
        store.record(rule("B", "Bash:npm install", true));
        let effects = vec![effect("A", 20, 100, 18, 100), effect("B", 20, 100, 2, 100)];
        let r = render(&store, &effects, 120, 30);
        assert!(
            r.body.contains("did **not** fall further"),
            "a null result must be stated, not buried:\n{}",
            r.body
        );
    }

    #[test]
    fn missing_control_is_called_out_rather_than_ignored() {
        let mut store = Store::default();
        store.record(rule("A", "Bash:git show", false));
        let r = render(&store, &[effect("A", 19, 129, 1, 84)], 120, 30);
        assert!(r.body.contains("No control arm"));
    }

    #[test]
    fn empty_store_explains_the_wait() {
        let r = render(&Store::default(), &[], 5, 1);
        assert!(r.body.contains("two to three weeks"));
    }
}
