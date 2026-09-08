//! The normalized shape of an agent session.
//!
//! Deliberately independent of Claude Code's transcript format. The failure
//! patterns this tool learns are not specific to one agent - Codex, OpenCode and
//! Hermes all leave logs, and all of them repeat their mistakes. Keeping the
//! model separate from the reader means a second agent is a new `read_*` module
//! rather than a rewrite.
//!
//! The unit is the **query**: one user message and everything the agent did in
//! response. Sessions are lists of queries.

use serde::{Deserialize, Serialize};

/// One tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub idx: usize,
    pub tool: String,
    /// The file path or command the tool acted on, when there is one.
    pub target: Option<String>,
    pub failed: bool,
    /// Error text when `failed`, verbatim. Normalisation happens in `signature`,
    /// not here - the raw text is what a human reads to judge whether a rule is
    /// justified, so it must survive.
    pub error: Option<String>,
}

/// One user message and the agent's response to it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Query {
    pub idx: usize,
    pub text: String,
    pub steps: Vec<Step>,
    pub tokens: u64,
    pub cost_usd: f64,
}

impl Query {
    /// Failures within this one query.
    pub fn failures(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.failed)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    /// Directory-encoded project name from the transcript path.
    pub project: String,
    /// Modification time, seconds since epoch. Used for before/after windows in
    /// `anomalift effect`, so it must be the file's own time rather than now().
    pub mtime: u64,
    pub queries: Vec<Query>,
}

impl Session {
    pub fn steps(&self) -> impl Iterator<Item = &Step> {
        self.queries.iter().flat_map(|q| q.steps.iter())
    }

    pub fn failures(&self) -> impl Iterator<Item = &Step> {
        self.steps().filter(|s| s.failed)
    }
}

/// A failure class: one normalised signature and every occurrence of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub signature: String,
    pub tool: String,
    pub count: usize,
    /// Sessions the pattern appeared in. A signature that fires ten times in one
    /// session is a single bad afternoon; one that fires across ten sessions is
    /// a habit, and only habits are worth a rule.
    pub sessions: usize,
    /// Verbatim examples, capped. Shown to the user so they can judge the rule.
    pub examples: Vec<String>,
    /// The user requests the agent was working on when this failed.
    ///
    /// A failure signature says *what* broke; this says *when*. "ripgrep
    /// rejected the pattern" during "find every call site of X" is a different
    /// situation from the same error during "rename this file", and a rule that
    /// knows the context can be conditioned rather than blanket. Capped, and
    /// truncated - these are for a human to recognise, not to store verbatim.
    #[serde(default)]
    pub queries: Vec<String>,
    pub first_seen: u64,
    pub last_seen: u64,
}
