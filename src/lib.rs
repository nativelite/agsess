//! agsess: local agent session state, from the files the user's own agent
//! CLIs write to their own disk.
//!
//! A session root (a directory path) in → a snapshot of [`AgentSession`]s out,
//! each carrying a derived attention [`Status`]. One concern, cleanly: **files
//! → session state.** It does not render, does not own a loop, does not spawn
//! or control anything, and never writes. Read-only, on the user's own disk,
//! documented surfaces only (Claude Code's `~/.claude/projects/**.jsonl`).
//!
//! ```no_run
//! let mut world = agsess::World::new(agsess::default_root());
//! world.refresh();
//! for s in &world.sessions {
//!     println!("{:?} {} -> {:?}", s.vendor, s.id, s.status);
//! }
//! ```
//!
//! The two modules are the seam: [`claude`] is the (unstable, vendor-specific)
//! format adapter, isolated behind fixtures; [`sessions`] is discovery,
//! incremental tailing, and the [`Status`] derivation. Zero third-party
//! dependencies: depends only on the org crate `json`.

pub mod aider;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor_agent;
pub mod gemini;
pub mod goose;
pub mod opencode;
pub mod qwen;
pub mod sessions;

pub use sessions::{AgentSession, Status, Vendor, World, APPROVAL_DWELL_MS, IDLE_MS};

use std::path::PathBuf;

/// The default Claude Code projects root: `~/.claude/projects`, resolved from
/// `USERPROFILE` (Windows) or `HOME` (Unix). Both apps in the suite need it,
/// so it lives here rather than in either binary.
pub fn default_root() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".claude").join("projects")
}
