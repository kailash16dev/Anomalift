//! Agent sources.
//!
//! Everything above this module is agent-agnostic by construction. `signature`
//! normalises error text and knows nothing about who produced it; `learn`
//! clusters `Step { tool, error }` and knows nothing about transcript formats.
//! The only agent-specific code in the tool is a `Source` implementation.
//!
//! That is the whole point. Agents repeat their mistakes regardless of vendor,
//! and the lesson from a recurring `git` misuse is the same whether Claude,
//! Codex or another agent made it. Adding an agent should be one file and a
//! fixture, not a fork.
//!
//! **What is supported, and why nothing more:**
//!
//! - **Claude Code** — CLI *and* the VS Code extension. The extension is
//!   `anthropic.claude-code`; it is the same program and writes the same
//!   `~/.claude/projects/**/*.jsonl`, so one reader covers both surfaces.
//! - **Codex, Cursor, others** — not implemented. Codex is not installed on the
//!   machine this was built on, so its on-disk format could not be verified, and
//!   Cursor stores conversations in an undocumented SQLite blob that changes
//!   between releases. Writing a reader against a format guessed at rather than
//!   observed produces a parser that appears to work and silently drops data -
//!   the failure mode this project has already paid for once.
//!
//! To add one: implement `Source`, commit a redacted fixture, and assert the
//! query count and failure count against hand-verified values.

pub mod claude;

use std::path::PathBuf;

use anyhow::Result;

use crate::model::Session;

/// One agent's on-disk session history.
pub trait Source {
    /// Short identifier, used in output once more than one source exists.
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    /// Where this agent keeps its transcripts. May not exist.
    fn root(&self) -> PathBuf;

    /// Every transcript, oldest first, so callers can take the newest N.
    fn discover(&self) -> Result<Vec<PathBuf>>;

    /// Parse one transcript into the shared model.
    ///
    /// Implementations must skip malformed records rather than failing the file:
    /// a session still being written is normal, and refusing to read it breaks
    /// the tool exactly when it is most wanted.
    fn parse(&self, path: &std::path::Path) -> Result<Session>;

    /// Whether this agent's data is present on the machine at all.
    fn available(&self) -> bool {
        self.root().exists()
    }
}

/// Every source the build knows about.
///
/// Returned whether or not the agent is installed; callers filter on
/// `available()` so that "Codex not found" can be reported rather than silently
/// producing an empty result.
pub fn all() -> Vec<Box<dyn Source>> {
    vec![Box::new(claude::ClaudeCode)]
}

/// Sources with data on this machine.
pub fn available() -> Vec<Box<dyn Source>> {
    all().into_iter().filter(|s| s.available()).collect()
}
