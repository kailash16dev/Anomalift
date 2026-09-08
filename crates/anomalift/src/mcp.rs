//! MCP server — so the agent can consult its own history at the moment it
//! matters.
//!
//! `CLAUDE.md` rules arrive at turn one and have to survive in attention until
//! the turn where they apply, competing with everything else in context. That is
//! the weakest thing about the advisory approach, and it is why `anomalift
//! effect` exists to check whether it works at all.
//!
//! This closes the timing gap without the risk of a `PreToolUse` hook. The agent
//! *asks* before running something risky; nothing is intercepted, nothing is
//! blocked, and a wrong answer costs a sentence rather than a broken tool call.
//! Advice at the point of use beats advice at the top of the session.
//!
//! Three tools, deliberately: check, patterns, stats. A server with a dozen
//! tools spends the context it is meant to save.
//!
//! **Speed is a correctness property here.** A check happens before tool calls,
//! so it has to answer in milliseconds. Re-parsing 192MB of transcripts per call
//! would make the agent slower than the failures it is avoiding, so the server
//! reads only the cache the scan leaves behind and never touches transcripts.
//!
//! Protocol: JSON-RPC 2.0 over stdio, hand-rolled. MCP is a small surface -
//! initialize, tools/list, tools/call - and a framework for three tools would
//! cost more in binary size than it saves in code.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

use crate::effect::opportunity_key;
use crate::model::Pattern;
use crate::store::Store;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// A pattern plus its rule, as cached by the scan.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CachedPattern {
    #[serde(flatten)]
    pub pattern: Pattern,
    pub rule: Option<String>,
    pub recurring: bool,
    /// Command shapes this failure arose from.
    #[serde(default)]
    pub opportunity_keys: Vec<String>,
}

/// Write the cache the server reads. Called by the scan, not by the server.
pub fn write_cache(path: &Path, patterns: &[CachedPattern]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string(patterns)? + "\n")?;
    Ok(())
}

fn read_cache(path: &Path) -> Vec<CachedPattern> {
    // A missing or corrupt cache means "nothing known", never an error: the
    // server must not fail an agent's tool call because a scan has not been run.
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub struct Server {
    cache: Vec<CachedPattern>,
    store: Store,
    rules_file: PathBuf,
}

impl Server {
    pub fn new(rules_file: PathBuf) -> Self {
        let cache = read_cache(&Store::cache_path_for(&rules_file));
        let store = Store::load(&Store::path_for(&rules_file)).unwrap_or_default();
        Self {
            cache,
            store,
            rules_file,
        }
    }

    /// Has this command shape failed before?
    ///
    /// Matches on the opportunity key rather than the literal string, so
    /// `git show --name-only -s HEAD~3` matches a failure recorded against
    /// `git show HEAD~1`. Exact-string matching would almost never hit.
    fn check(&self, tool: &str, command: &str) -> Value {
        let key = opportunity_key(tool, Some(command));
        let mut hits: Vec<&CachedPattern> = self
            .cache
            .iter()
            .filter(|c| c.recurring && c.opportunity_keys.iter().any(|k| k == &key))
            .collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.pattern.count));

        if hits.is_empty() {
            return json!({
                "known_failures": 0,
                "advice": "No recorded failures for this command shape.",
            });
        }

        let findings: Vec<Value> = hits
            .iter()
            .take(3)
            .map(|h| {
                json!({
                    "failed_times": h.pattern.count,
                    "across_sessions": h.pattern.sessions,
                    "error": h.pattern.examples.first().cloned().unwrap_or_default(),
                    "fix": h.rule.clone(),
                })
            })
            .collect();

        json!({
            "known_failures": hits.len(),
            "command_shape": key,
            "findings": findings,
            // Stated rather than implied: the agent should not treat a match as
            // a prohibition. Most of these commands succeed most of the time.
            "advice": "This command shape has failed before. Check the fix below \
                       before running it; it is not a prohibition.",
        })
    }

    fn patterns(&self) -> Value {
        let recurring: Vec<Value> = self
            .cache
            .iter()
            .filter(|c| c.recurring)
            .map(|c| {
                json!({
                    "tool": c.pattern.tool,
                    "failed_times": c.pattern.count,
                    "across_sessions": c.pattern.sessions,
                    "error": c.pattern.examples.first().cloned().unwrap_or_default(),
                    "fix": c.rule.clone(),
                })
            })
            .collect();
        json!({ "recurring_patterns": recurring.len(), "patterns": recurring })
    }

    fn stats(&self) -> Value {
        let rules: Vec<Value> = self
            .store
            .rules
            .iter()
            .map(|r| {
                json!({
                    "rule": r.rule,
                    "applied_on": r.applied_on,
                    "baseline": format!("{}/{} attempts", r.baseline.failures, r.baseline.attempts),
                    "latest_verdict": r.latest().map(|m| m.verdict.clone()),
                })
            })
            .collect();
        json!({
            "rules_active": rules.len(),
            "rules": rules,
            "cached_patterns": self.cache.len(),
            "rules_file": self.rules_file.display().to_string(),
        })
    }

    fn tool_definitions() -> Value {
        json!([
            {
                "name": "anomalift_check",
                // Written as an instruction, not a description: a tool the model
                // has to infer a use for does not get used.
                "description": "Before running a shell command that has bitten you before, \
                                check whether this command shape has failed in past sessions. \
                                Returns the recorded error and the known fix. Fast, local, and \
                                advisory - it never blocks. Use it for git, ripgrep, file reads \
                                and anything with fiddly flags.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The command or file path you are about to use, e.g. 'git show --name-only -s HEAD'"
                        },
                        "tool": {
                            "type": "string",
                            "description": "Tool name: Bash, Read, Grep, Edit. Defaults to Bash."
                        }
                    },
                    "required": ["command"]
                }
            },
            {
                "name": "anomalift_patterns",
                "description": "List every mistake this agent repeats in this project, with how \
                                often and the known fix. Use when starting work in an unfamiliar \
                                repo, or when asked what goes wrong here.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "anomalift_stats",
                "description": "Show which prevention rules are active, their frozen baselines, \
                                and whether each has been measured to work.",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ])
    }

    fn call(&self, name: &str, args: &Value) -> Value {
        match name {
            "anomalift_check" => {
                let command = args.get("command").and_then(Value::as_str).unwrap_or("");
                let tool = args.get("tool").and_then(Value::as_str).unwrap_or("Bash");
                self.check(tool, command)
            }
            "anomalift_patterns" => self.patterns(),
            "anomalift_stats" => self.stats(),
            other => json!({ "error": format!("unknown tool: {other}") }),
        }
    }

    fn handle(&self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");

        let result = match method {
            "initialize" => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "anomalift", "version": env!("CARGO_PKG_VERSION") }
            }),
            "tools/list" => json!({ "tools": Self::tool_definitions() }),
            "tools/call" => {
                let params = req.get("params").cloned().unwrap_or(json!({}));
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let out = self.call(name, &args);
                json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&out).unwrap_or_default()
                    }]
                })
            }
            // Notifications have no id and expect no reply. Answering one is a
            // protocol error that some clients treat as fatal.
            _ if id.is_none() => return None,
            other => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {other}") }
                }))
            }
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    /// Read requests from stdin, write responses to stdout, one JSON per line.
    ///
    /// Nothing may be printed to stdout except protocol messages - a stray
    /// `println!` corrupts the stream and the client drops the connection with
    /// no useful error. Diagnostics go to stderr.
    pub fn serve(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        eprintln!(
            "anomalift mcp: {} cached patterns, {} active rules",
            self.cache.len(),
            self.store.rules.len()
        );
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(req) = serde_json::from_str::<Value>(&line) else {
                continue; // malformed input is the client's problem, not fatal
            };
            if let Some(resp) = self.handle(&req) {
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with(cache: Vec<CachedPattern>) -> Server {
        Server {
            cache,
            store: Store::default(),
            rules_file: PathBuf::from("CLAUDE.md"),
        }
    }

    fn cached(tool: &str, key: &str, count: usize) -> CachedPattern {
        CachedPattern {
            pattern: Pattern {
                signature: format!("{tool}: boom"),
                tool: tool.into(),
                count,
                sessions: 5,
                examples: vec!["fatal: options cannot be used together".into()],
                queries: vec![],
                first_seen: 0,
                last_seen: 0,
            },
            rule: Some("use --format= instead".into()),
            recurring: true,
            opportunity_keys: vec![key.into()],
        }
    }

    #[test]
    fn matches_on_shape_not_exact_string() {
        // The whole point: a failure recorded against one invocation must warn
        // about a different invocation of the same command.
        let s = server_with(vec![cached("Bash", "Bash:git show", 19)]);
        let out = s.check("Bash", "git show --name-only -s HEAD~3");
        assert_eq!(out["known_failures"], 1);
        assert_eq!(out["findings"][0]["failed_times"], 19);
    }

    #[test]
    fn unrelated_commands_are_silent() {
        // False warnings are worse than none: an agent that gets noise here will
        // stop calling the tool.
        let s = server_with(vec![cached("Bash", "Bash:git show", 19)]);
        assert_eq!(s.check("Bash", "git commit -m wip")["known_failures"], 0);
        assert_eq!(s.check("Bash", "npm install")["known_failures"], 0);
    }

    #[test]
    fn non_recurring_patterns_do_not_warn() {
        let mut c = cached("Bash", "Bash:git show", 2);
        c.recurring = false;
        assert_eq!(
            server_with(vec![c]).check("Bash", "git show HEAD")["known_failures"],
            0
        );
    }

    #[test]
    fn missing_cache_is_not_an_error() {
        assert!(read_cache(Path::new("/nonexistent/patterns.json")).is_empty());
        assert_eq!(
            server_with(vec![]).check("Bash", "git show")["known_failures"],
            0
        );
    }

    #[test]
    fn notifications_get_no_reply() {
        let s = server_with(vec![]);
        let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(s.handle(&notif).is_none());
    }

    #[test]
    fn initialize_and_list_are_well_formed() {
        let s = server_with(vec![]);
        let init = s
            .handle(&json!({ "jsonrpc":"2.0","id":1,"method":"initialize" }))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        let list = s
            .handle(&json!({ "jsonrpc":"2.0","id":2,"method":"tools/list" }))
            .unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);
    }
}
