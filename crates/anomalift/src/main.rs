mod effect;
mod hook;
mod learn;
mod mcp;
mod model;
mod rules;
mod share;
mod signature;
mod source;
mod store;
mod term;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "anomalift",
    about = "Learn what your agent gets wrong, and stop it",
    long_about = "Run `anomalift` with no arguments to scan your agent's sessions for \
mistakes it keeps repeating. Three commands cover the whole loop: `anomalift` finds them, \
`anomalift apply` writes rules that prevent them, `anomalift effect` measures whether those rules \
worked.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    // Repeated from `Learn` so the default command accepts its own flags.
    // Without these, `anomalift --sessions 200` fails with "unexpected argument"
    // while bare `anomalift` works - the most common invocation refusing its most
    // useful option.
    /// How many recent sessions to read.
    #[arg(long, default_value_t = 120, global = true)]
    sessions: usize,
    /// Only this project's sessions.
    #[arg(long, global = true)]
    project: Option<String>,
    /// Include patterns below the recurrence thresholds.
    #[arg(long, global = true)]
    all: bool,
    /// Emit JSON rather than a report.
    #[arg(long, global = true)]
    json: bool,
    /// Rules file. Only used to decide where the `.anomalift/` cache lands.
    /// Defaults to ./CLAUDE.md
    #[arg(long, global = true)]
    file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Record one tool failure, read as a JSON hook payload on stdin.
    ///
    /// Wired to Claude Code's `PostToolUseFailure` event, this appends to
    /// `.anomalift/observed.jsonl`. It runs inside every failing tool call, so
    /// it prints nothing, blocks on nothing, and exits 0 whatever goes wrong.
    ///
    /// Nothing reads that log yet: the scan still works from transcripts, and
    /// merging the two sources without deduplication would double-count every
    /// failure recorded in both. Installing the hook is a separate, deliberate
    /// step - this subcommand only records.
    Hook {
        /// Rules file whose `.anomalift/` directory receives the record.
        /// Defaults to the session's own `cwd`, taken from the payload.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Run as an MCP server so the agent can consult failures at the moment of
    /// a tool call, rather than relying on a rule read at session start.
    Mcp {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Write the proposed rules into CLAUDE.md, after showing the diff.
    Apply {
        /// Rules file. Defaults to ./CLAUDE.md
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value_t = 120)]
        sessions: usize,
        #[arg(long)]
        project: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Show the diff and exit without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the block anomalift added to CLAUDE.md, and nothing else.
    Forget {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    /// Write a redacted report you can send to whoever is collecting beta results.
    Share {
        #[arg(long)]
        file: Option<PathBuf>,
        /// Where to write it. Defaults to ./anomalift-report.md
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 400)]
        sessions: usize,
        /// Only this project's sessions.
        #[arg(long)]
        project: Option<String>,
    },
    /// Did a rule work? Compares failure *rates* before and after a cutoff.
    Effect {
        /// Cutoff date, YYYY-MM-DD. Defaults to the date recorded in the
        /// CLAUDE.md block, so the split is when rules were actually written
        /// rather than a date typed from memory.
        #[arg(long)]
        since: Option<String>,
        /// Rules file to read the cutoff from. Defaults to ./CLAUDE.md
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value_t = 400)]
        sessions: usize,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Find failures your agent repeats. Same as running `anomalift` with no arguments.
    #[command(hide = true)]
    Learn {
        /// How many recent sessions to read.
        #[arg(long, default_value_t = 120)]
        sessions: usize,
        /// Only this project's sessions.
        #[arg(long)]
        project: Option<String>,
        /// Include patterns below the recurrence thresholds.
        #[arg(long)]
        all: bool,
        /// Emit JSON rather than a report.
        #[arg(long)]
        json: bool,
    },
}

/// Read the N most recent transcripts, newest last.
///
/// Failures to parse one file are skipped rather than fatal: a session still
/// being written is normal, and refusing to read anything because one file is
/// half-flushed would break the tool exactly when it is most wanted.
/// Reads from every agent present on the machine, not just one. A recurring
/// `git` misuse is the same lesson whichever agent made it, so patterns are
/// learned across all of them and the rule written once.
fn load(sessions: usize, project: Option<&str>) -> Result<Vec<model::Session>> {
    let mut out = Vec::new();
    for src in source::available() {
        let mut paths = src.discover()?;
        if let Some(p) = project {
            paths.retain(|path| {
                path.parent()
                    .and_then(|d| d.file_name())
                    .map(|n| n.to_string_lossy().contains(p))
                    .unwrap_or(false)
            });
        }
        // Newest N per source, so one agent with a long history cannot crowd
        // another out of the window entirely.
        if paths.len() > sessions {
            paths.drain(..paths.len() - sessions);
        }
        out.extend(paths.iter().filter_map(|p| src.parse(p).ok()));
    }
    out.sort_by_key(|s: &model::Session| s.mtime);
    Ok(out)
}

/// `YYYY-MM-DD` to a unix timestamp at UTC midnight.
///
/// Hand-rolled rather than pulling `chrono` for one conversion: the binary is
/// meant to stay small, and this is the only date arithmetic in the tool.
fn parse_date(s: &str) -> Result<u64> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        anyhow::bail!("expected YYYY-MM-DD, got {s:?}");
    }
    let y: i64 = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("bad year in {s:?}"))?;
    let m: i64 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("bad month in {s:?}"))?;
    let d: i64 = parts[2]
        .parse()
        .map_err(|_| anyhow::anyhow!("bad day in {s:?}"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        anyhow::bail!("out-of-range date {s:?}");
    }
    // Day-count arithmetic accepts 2026-02-31 and silently rolls it forward,
    // so a typo becomes a cutoff three days from the one asked for.
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let max_day = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if leap {
                29
            } else {
                28
            }
        }
    };
    if d > max_day {
        anyhow::bail!("{s:?} is not a real date");
    }
    // Howard Hinnant's days-from-civil: correct across leap years and centuries,
    // which a naive 365.25 approximation is not.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days.max(0) * 86_400).map_err(|_| anyhow::anyhow!("date out of range"))
}

/// Ask before touching a file the user owns.
///
/// A non-interactive stdin (piped, CI) answers no rather than yes: defaulting to
/// yes would mean a script that happens to invoke `anomalift apply` silently edits a
/// developer's config.
fn confirm(prompt: &str) -> bool {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        eprintln!("not a terminal; refusing to write without --yes");
        return false;
    }
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes")
}

fn rules_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| PathBuf::from("CLAUDE.md"))
}

fn main() -> Result<()> {
    // No subcommand means the thing the tool is for. A user should not have to
    // learn a verb before the tool has shown them anything useful.
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Learn {
        sessions: cli.sessions,
        project: cli.project.clone(),
        all: cli.all,
        json: cli.json,
    });
    match command {
        Command::Hook { file } => {
            // Nothing may escape this arm: no `?`, no output, no non-zero exit.
            // A hook that fails loudly degrades every turn of the session it is
            // meant to be quietly learning from.
            hook::run(file);
        }
        Command::Mcp { file } => {
            mcp::Server::new(rules_path(file)).serve()?;
        }
        Command::Apply {
            file,
            sessions,
            project,
            yes,
            dry_run,
        } => {
            let path = rules_path(file);
            let loaded = load(sessions, project.as_deref())?;
            let pats = learn::patterns(&loaded);
            // Only recurring patterns with a remedy the tool is sure of. A
            // pattern with no known fix is reported by `anomalift` and never
            // written here, because a wrong line misleads the agent every turn.
            // Distinguish "nothing recurs" from "nothing to read". Both used to
            // print "no recurring pattern has a known fix", which tells a user
            // with an empty history the wrong thing about their own machine.
            if loaded.is_empty() {
                println!(
                    "\n  No agent transcripts found in ~/.claude/projects.\n\n  \
                     Anomalift learns from sessions you have already run.\n  \
                     Use your agent for a while, then run this again.\n"
                );
                return Ok(());
            }

            let proposed: Vec<String> = learn::recurring(&pats)
                .iter()
                .filter_map(|p| learn::rule_for(p))
                .collect();

            let rf = rules::RuleFile::open(&path)?;
            if let Some(why) = rf.refusal() {
                // Never guess. This edits a file the user wrote by hand.
                eprintln!(
                    "\n  refusing to touch {}: {why}.\n  \
                     Fix the anomalift markers by hand, or delete them, then run this again.\n",
                    path.display()
                );
                std::process::exit(1);
            }
            // Freeze baselines first, and freeze them even when no rule can be
            // written. Returning early here meant a user whose failures have no
            // known remedy - there are only six - got no `learned.json` at all
            // and their `share` report came back empty in every section. Their
            // control data is exactly as valuable as anyone else's.
            let store_path = store::Store::path_for(&path);
            let mut store = store::Store::load(&store_path)?;
            let today = rules::today();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut newly = 0usize;
            for p in learn::recurring(&pats) {
                let rule = learn::rule_for(p);
                let control = rule.is_none();
                let rule = rule.unwrap_or_default();
                let stats = effect::baseline_for(&loaded, &p.signature);
                if store.record(store::LearnedRule {
                    signature: p.signature.clone(),
                    tool: p.tool.clone(),
                    opportunity_keys: stats.keys,
                    rule,
                    control,
                    example_queries: p.queries.clone(),
                    applied_on: today.clone(),
                    applied_at: now,
                    baseline_sessions: stats.sessions.clone(),
                    baseline: store::Baseline {
                        failures: stats.failures,
                        attempts: stats.attempts,
                        sessions_scanned: loaded.len(),
                    },
                    measurements: Vec::new(),
                }) {
                    newly += 1;
                }
            }
            if newly > 0 {
                store.save(&store_path)?;
            }

            if proposed.is_empty() {
                println!("\n  no recurring pattern has a known fix, so no rule was written.");
                if newly > 0 {
                    println!("  froze {newly} baselines anyway — they are the control arm,");
                    println!("  and `anomalift share` still has something to report.\n");
                } else {
                    println!("  nothing recurring to measure yet.\n");
                }
                return Ok(());
            }
            let next = rf.rendered(&proposed, &rules::today());
            if rf.unchanged(&next) {
                println!("\n  {} is already up to date\n", path.display());
                return Ok(());
            }

            println!("\n  {}\n", path.display());
            print!("{}", rules::diff_display(&rf.current(), &next));
            println!();

            if dry_run {
                println!("  dry run — nothing written\n");
                return Ok(());
            }
            if !yes
                && !confirm(&format!(
                    "  write {} rules to {}?",
                    proposed.len(),
                    path.display()
                ))
            {
                println!("  cancelled\n");
                return Ok(());
            }
            let backup = rf.write(&next)?;

            println!(
                "\n  wrote {}",
                if proposed.len() == 1 {
                    "1 rule".to_string()
                } else {
                    format!("{} rules", proposed.len())
                }
            );
            println!(
                "  froze {} in {}",
                if newly == 1 {
                    "1 baseline".to_string()
                } else {
                    format!("{newly} baselines")
                },
                store_path.display()
            );
            if let Some(b) = backup {
                println!("  backup: {}", b.display());
            }
            // Not `--since`: with baselines frozen, `effect` ignores it and
            // says so. Printing it here sends the user straight into a warning.
            println!("  measure it later with: anomalift effect\n");
        }
        Command::Forget { file, yes } => {
            let path = rules_path(file);
            let rf = rules::RuleFile::open(&path)?;
            if let Some(why) = rf.refusal() {
                // Never guess. This edits a file the user wrote by hand.
                eprintln!(
                    "\n  refusing to touch {}: {why}.\n  \
                     Fix the anomalift markers by hand, or delete them, then run this again.\n",
                    path.display()
                );
                std::process::exit(1);
            }
            // Nothing is destroyed before consent. An earlier version cleared
            // the store above this point to fix a different bug - baselines
            // surviving when the block was already gone - and in doing so wiped
            // the frozen baselines even when the user answered "no". They have
            // no backup, unlike CLAUDE.md, so that was unrecoverable loss of the
            // one artifact this tool exists to produce.
            let store_path = store::Store::path_for(&path);
            let mut store = store::Store::load(&store_path).unwrap_or_default();
            let stored = store.rules.len();

            let next = rf.without_block();
            let block_present = !rf.unchanged(&next);

            if !block_present && stored == 0 {
                println!("\n  nothing to forget in {}\n", path.display());
                return Ok(());
            }

            println!("\n  {}\n", path.display());
            if block_present {
                print!("{}", rules::diff_display(&rf.current(), &next));
            } else {
                // The block is gone already - hand-edited, reformatted, merged
                // away - but the baselines remain and are still being measured.
                println!(
                    "  no anomalift block in the file, but {stored} frozen \
                          baselines remain and are still being reported by \
                          `effect` and `share`."
                );
            }
            if stored > 0 {
                println!("\n  this also discards {stored} frozen baselines. They cannot be");
                println!("  recovered, and re-applying starts the measurement clock again.");
            }
            println!();

            if !yes && !confirm("  proceed?") {
                println!("  cancelled — nothing was changed\n");
                return Ok(());
            }

            let mut backup = None;
            if block_present {
                backup = rf.write(&next)?;
            }
            if stored > 0 {
                store.remove_all();
                store.save(&store_path)?;
            }

            println!();
            if block_present {
                println!("  removed the anomalift block");
            }
            if stored > 0 {
                println!("  cleared {stored} frozen baselines");
            }
            if let Some(b) = backup {
                println!("  backup: {}", b.display());
            }
            println!();
        }
        Command::Share {
            file,
            out,
            sessions,
            project,
        } => {
            let rules_file = rules_path(file);
            let store = store::Store::load(&store::Store::path_for(&rules_file))?;
            // Honour --project. It used to be advertised in --help and then
            // dropped here, so a user who scoped the report they were about to
            // send a stranger had not scoped it at all.
            let loaded = load(sessions, project.as_deref())?;
            let effects = effect::measure_stored(&loaded, &store);
            let days = match (loaded.first(), loaded.last()) {
                (Some(a), Some(b)) => (b.mtime.saturating_sub(a.mtime)) / 86_400,
                _ => 0,
            };
            // An empty store means `apply` was never run here - most often the
            // user is in a different directory than the one they applied from.
            // Writing a report with nothing in it and calling it "safe to send"
            // is technically true and completely useless.
            if store.rules.is_empty() {
                println!(
                    "\n  Nothing to report yet: no frozen baselines in {}.\n\n  \
                     `anomalift share` reports on rules written by `anomalift apply`,\n  \
                     and reads the baselines beside the CLAUDE.md they were written for.\n  \
                     Run `anomalift apply` first, from the directory holding that file.\n",
                    store::Store::path_for(&rules_file).display()
                );
                return Ok(());
            }

            let mut report = share::render(&store, &effects, loaded.len(), days);
            let path = out.unwrap_or_else(|| PathBuf::from("anomalift-report.md"));
            share::write(&report, &path)?;
            report.path = path.clone();
            println!("\n  wrote {}", path.display());
            println!(
                "  {} ruled patterns · {} controls",
                report.ruled, report.controls
            );
            println!("  no paths, queries or error text - safe to send as-is\n");
        }
        Command::Effect {
            since,
            file,
            sessions,
            project,
            json,
        } => {
            // Prefer frozen baselines. Falling back to a recomputed window is
            // supported for a user who has not run `anomalift apply`, but it is the
            // weaker measurement and says so.
            let rules_file = rules_path(file.clone());
            let store_path = store::Store::path_for(&rules_file);
            let mut store = store::Store::load(&store_path)?;
            if !store.rules.is_empty() {
                if since.is_some() {
                    // Silently measuring against a different cutoff than the
                    // one asked for is how a user ends up trusting a number
                    // that answers a question they did not pose.
                    eprintln!(
                        "  note: --since ignored; measuring against the baselines frozen \
                         by `anomalift apply`, which is the only comparison that is valid.\n  \
                         Use `anomalift forget` first if you want a fresh cutoff."
                    );
                }
                let loaded = load(sessions, project.as_deref())?;
                let effects = effect::measure_stored(&loaded, &store);
                if json {
                    println!("{}", serde_json::to_string(&effect::as_rows(&effects))?);
                } else {
                    term::effect_report(&loaded, &effects, "frozen baselines");
                }
                // Keep a dated point per rule. A single run says where a rule
                // stands today; the series says whether it is drifting back,
                // which is the question a beta run is actually asking. Repeat
                // runs on one day replace rather than stack, so this cannot
                // grow without bound.
                let today = rules::today();
                for e in &effects {
                    store.add_measurement(
                        &e.signature,
                        store::Measurement {
                            on: today.clone(),
                            failures: e.after_failures,
                            attempts: e.after_attempts,
                            p_value: e.p_value,
                            verdict: effect::verdict_word(&e.verdict).to_string(),
                        },
                    );
                }
                // Best-effort: a read-only diagnostic must not fail because a
                // history point could not be appended.
                let _ = store.save(&store_path);
                return Ok(());
            }
            let since = match since {
                Some(s) => s,
                None => {
                    let path = rules_path(file);
                    rules::RuleFile::open(&path)?.applied_on().ok_or_else(|| {
                        anyhow::anyhow!(
                            "no anomalift block in {} to take a cutoff from — pass --since YYYY-MM-DD, \
                             or run `anomalift apply` first",
                            path.display()
                        )
                    })?
                }
            };
            let cutoff = parse_date(&since)?;
            let loaded = load(sessions, project.as_deref())?;
            let effects = effect::measure(&loaded, cutoff);
            if json {
                println!("{}", serde_json::to_string(&effect::as_rows(&effects))?);
            } else {
                term::effect_report(&loaded, &effects, &since);
            }
        }
        Command::Learn {
            sessions,
            project,
            all,
            json,
        } => {
            let loaded = load(sessions, project.as_deref())?;
            let pats = learn::patterns(&loaded);
            if json {
                // The rule travels with the pattern so consumers never
                // reimplement the remedy table. A second copy in the editor
                // extension would drift from this one and give two different
                // answers to the same question.
                #[derive(serde::Serialize)]
                struct Row<'a> {
                    #[serde(flatten)]
                    pattern: &'a model::Pattern,
                    rule: Option<String>,
                    recurring: bool,
                }
                let rows: Vec<Row> = pats
                    .iter()
                    .map(|p| Row {
                        pattern: p,
                        rule: learn::rule_for(p),
                        recurring: p.count >= learn::MIN_OCCURRENCES
                            && p.sessions >= learn::MIN_SESSIONS,
                    })
                    .collect();
                println!("{}", serde_json::to_string(&rows)?);
            } else {
                term::report(&loaded, &pats, all);
            }
            // Leave a cache so `anomalift mcp` can answer per-tool-call without
            // re-reading every transcript.
            let cache: Vec<mcp::CachedPattern> = pats
                .iter()
                .map(|p| mcp::CachedPattern {
                    pattern: p.clone(),
                    rule: learn::rule_for(p),
                    recurring: p.count >= learn::MIN_OCCURRENCES
                        && p.sessions >= learn::MIN_SESSIONS,
                    opportunity_keys: effect::baseline_for(&loaded, &p.signature)
                        .keys
                        .into_iter()
                        .collect(),
                })
                .collect();
            let cache_path = store::Store::cache_path_for(&PathBuf::from("CLAUDE.md"));
            if let Err(e) = mcp::write_cache(&cache_path, &cache) {
                eprintln!("warning: could not write pattern cache: {e}");
            }
        }
    }
    Ok(())
}
