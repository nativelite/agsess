//! Session discovery, incremental tailing, and attention-status derivation.
//!
//! A [`World`] scans a Claude Code projects root
//! (`~/.claude/projects/*/<session>.jsonl`), keeps one [`AgentSession`] per
//! transcript, and on each [`refresh`](World::refresh) reads only the bytes
//! appended since last time; a partial trailing line is buffered until its
//! newline arrives. A truncated/rewritten file resets and re-aggregates.
//!
//! Ported from agtop's `sessions.rs` (`Session` renamed [`AgentSession`]),
//! then extended with [`Vendor`], the attention [`Status`] derived from the
//! transcript tail (§2.4 of the atrium-0.3 design), a `first_seen_ms` stamp, and
//! [`World::refresh_since`] for a bounded cold start. The incremental-tail
//! state (`offset`, `partial`) and the status-tracking fields stay private:
//! that machinery is the crate's value and neither app should reimplement it.

use crate::claude::{self, Kind, Tail};
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which agent CLI produced a session. An enum, not a trait, in v1: `Vendor`
/// is on every session from day one so the UI/binder never change when a
/// second vendor lands, and adding a variant produces a compile error at
/// exactly the `match` sites that must change (§6 of the design).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    ClaudeCode,
    Gemini,
    Codex,
    Aider,
    CursorAgent,
    Copilot,
    Qwen,
    OpenCode,
    Goose,
}

impl Vendor {
    /// Dispatch one transcript line to the vendor's format adapter. This is the
    /// integration seam: every vendor's `parse_line` returns the shared
    /// [`claude::LineEvent`], so the tailing and status machinery stay
    /// vendor-blind. Adding a variant fails to compile here until it is wired,
    /// by design (§6): the enum forces the dispatch to stay total.
    pub fn parse_line(self, line: &str) -> Option<claude::LineEvent> {
        match self {
            Vendor::ClaudeCode => claude::parse_line(line),
            Vendor::Gemini => crate::gemini::parse_line(line),
            Vendor::Codex => crate::codex::parse_line(line),
            Vendor::Aider => crate::aider::parse_line(line),
            Vendor::CursorAgent => crate::cursor_agent::parse_line(line),
            Vendor::Copilot => crate::copilot::parse_line(line),
            Vendor::Qwen => crate::qwen::parse_line(line),
            Vendor::OpenCode => crate::opencode::parse_line(line),
            Vendor::Goose => crate::goose::parse_line(line),
        }
    }

    /// The transcript file extension the generic discovery scan matches for this
    /// vendor. Most vendors write JSONL; aider writes a markdown chat log.
    ///
    /// NOTE: opencode and cursor-agent (main sessions) persist to SQLite, not a
    /// tailable text file, so the file scan never finds them and their discovery
    /// is deferred: the file scan cannot reach a SQLite store. Their `parse_line`
    /// is still wired above and
    /// unit-tested against synthetic lines.
    fn transcript_ext(self) -> &'static str {
        match self {
            Vendor::Aider => "md",
            _ => "jsonl",
        }
    }

    /// Whether this vendor stamps every transcript turn with a timestamp. The
    /// JSONL vendors and Claude Code do; aider's markdown log carries only a
    /// session-start time, so its activity clock must fall back to the file's
    /// mtime (see [`AgentSession::derive_status`]) or every aider session traps
    /// in `Idle` after `IDLE_MS`.
    fn has_line_timestamps(self) -> bool {
        !matches!(self, Vendor::Aider)
    }
}

/// The attention status derived from a session's transcript tail. This is the
/// public contract; *how* it is derived (the dwell heuristic below) is private
/// to the adapter and may be replaced by a hook-fed path later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// An assistant turn / tool is in flight.
    Working,
    /// A tool call was requested and not resolved, in a mode that prompts,
    /// and it has dwelled long enough to be a human wait (not a slow tool).
    WaitingApproval,
    /// The turn ended cleanly on a text block; the agent wants a human message.
    WaitingPrompt,
    /// Nothing recent enough to claim either way.
    Idle,
}

/// Dwell before an unresolved `tool_use` in a prompting mode is read as a
/// human wait rather than a slow tool. 6–8 s band (founder call); tunable.
pub const APPROVAL_DWELL_MS: u64 = 7_000;

/// A session with no activity within this window of `now` is [`Status::Idle`].
/// One minute: long enough not to flap a briefly-quiet agent, short enough
/// that a truly stopped session greys out promptly.
pub const IDLE_MS: u64 = 60_000;

/// Permission modes that actually prompt the human on a tool call. Only in
/// these does an unresolved `tool_use` past the dwell escalate to
/// `WaitingApproval`; the auto-approving modes stay `Working`.
fn mode_prompts(mode: Option<&str>) -> bool {
    matches!(mode, Some("default") | Some("plan"))
}

/// Aggregated view of one session transcript.
#[derive(Debug)]
pub struct AgentSession {
    pub vendor: Vendor,
    pub path: PathBuf,
    pub id: String,
    pub project_dir: String,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub status: Status,
    pub user_msgs: u64,
    pub assistant_msgs: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub last_ts_ms: Option<u64>,
    pub last_action: String,
    /// Most recent action lines, newest last (bounded).
    pub recent: VecDeque<String>,
    pub mtime_ms: u64,
    /// When this process first observed the transcript file (§3 of the design).
    pub first_seen_ms: u64,
    pub subagents_total: usize,
    pub subagents_active: usize,
    offset: u64,
    partial: Vec<u8>,
    /// Bytes up to the next newline belong to a line we dropped (it outgrew
    /// [`TAIL_CAP`], or a capped read started inside it). `partial` is empty
    /// whenever this is set.
    skip_line: bool,
    // --- private status-derivation state, updated line by line ---
    /// Last permission mode seen (from `permission-mode` records or the
    /// inline `permissionMode` on `user` lines).
    perm_mode: Option<String>,
    /// The kind of the most recent user/assistant line.
    last_kind: Option<Kind>,
    /// The tail shape of the most recent user/assistant line.
    last_tail: Tail,
    /// Ids of `tool_use` calls not yet matched by a `tool_result`.
    open_tools: Vec<String>,
    /// Timestamp (ms) of the currently-unresolved trailing `tool_use`, if the
    /// last line is that call: the anchor for the dwell gate.
    pending_tool_ts: Option<u64>,
}

const RECENT_KEEP: usize = 6;
/// A subagent transcript touched within this window counts as active.
const SUBAGENT_ACTIVE_MS: u64 = 120_000;
/// Upper bound on a *single* tail read. A monitor only needs the recent tail
/// of a transcript to derive status, so when a session is further behind than
/// this (a huge backlog, or the first read of a large live transcript) we read
/// only the last `TAIL_CAP` bytes and resync at the next line boundary instead
/// of `read_to_end`-ing hundreds of MB and blocking the caller's loop. It also
/// bounds `partial`: a single line longer than this is skipped, not buffered.
/// 4 MiB clears the longest real lines (image tool results reach ~3 MiB; none
/// of 604k measured lines exceeded 4 MiB), so no real event is skipped, while
/// keeping a monitor's largest routine allocation small: 16 MiB was the one
/// that failed first when a fleet's builds exhausted system commit.
const TAIL_CAP: u64 = 4 * 1024 * 1024;
/// Upper bound on `open_tools`. A tool call interrupted before its result never
/// resolves, so without a bound the list grows for the life of the session.
const MAX_OPEN_TOOLS: usize = 256;

impl AgentSession {
    fn new(vendor: Vendor, path: PathBuf, id: String, project_dir: String) -> AgentSession {
        AgentSession {
            vendor,
            path,
            id,
            project_dir,
            cwd: None,
            branch: None,
            model: None,
            status: Status::Idle,
            user_msgs: 0,
            assistant_msgs: 0,
            tokens_in: 0,
            tokens_out: 0,
            last_ts_ms: None,
            last_action: String::new(),
            recent: VecDeque::new(),
            mtime_ms: 0,
            first_seen_ms: 0,
            subagents_total: 0,
            subagents_active: 0,
            offset: 0,
            partial: Vec::new(),
            skip_line: false,
            perm_mode: None,
            last_kind: None,
            last_tail: Tail::None,
            open_tools: Vec::new(),
            pending_tool_ts: None,
        }
    }

    /// Display name: the basename of the session's working directory when
    /// known (the transcript records the real path), else the storage
    /// directory name Claude Code munged the path into.
    pub fn project_name(&self) -> String {
        match &self.cwd {
            Some(cwd) => Path::new(cwd)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| cwd.clone()),
            None => self.project_dir.clone(),
        }
    }

    fn reset(&mut self) {
        let (path, id, dir) = (self.path.clone(), self.id.clone(), self.project_dir.clone());
        let first_seen = self.first_seen_ms;
        *self = AgentSession::new(self.vendor, path, id, dir);
        self.first_seen_ms = first_seen; // a rewrite is still the same observed file
    }

    fn apply(&mut self, ev: claude::LineEvent) {
        match ev.kind {
            Kind::User => self.user_msgs += 1,
            Kind::Assistant => self.assistant_msgs += 1,
            Kind::Other => {}
        }
        self.tokens_in += ev.tokens_in;
        self.tokens_out += ev.tokens_out;
        if ev.ts_ms.is_some() {
            self.last_ts_ms = ev.ts_ms;
        }
        if let Some(m) = ev.model {
            self.model = Some(m);
        }
        if let Some(c) = ev.cwd {
            self.cwd = Some(c);
        }
        if let Some(b) = ev.branch {
            self.branch = Some(b);
        }
        if let Some(pm) = ev.permission_mode {
            self.perm_mode = Some(pm);
        }
        // --- status tracking (§2.4) -------------------------------------
        // Resolve/track tool calls regardless of which line they arrive on.
        match &ev.tail {
            Tail::ToolUse(id) => {
                if !id.is_empty() {
                    if self.open_tools.len() == MAX_OPEN_TOOLS {
                        self.open_tools.remove(0); // the oldest: likely interrupted
                    }
                    self.open_tools.push(id.clone());
                }
            }
            Tail::ToolResult(id) => {
                if let Some(pos) = self.open_tools.iter().position(|t| t == id) {
                    self.open_tools.remove(pos);
                }
            }
            Tail::Text | Tail::None => {}
        }
        // The last user/assistant line drives the tail-shape derivation.
        if matches!(ev.kind, Kind::User | Kind::Assistant) {
            self.last_kind = Some(ev.kind);
            self.last_tail = ev.tail.clone();
            self.pending_tool_ts = match &ev.tail {
                Tail::ToolUse(id) if self.open_tools.iter().any(|t| t == id) => ev.ts_ms,
                _ => None,
            };
        }
        if let Some(a) = ev.action {
            self.last_action = a.clone();
            if self.recent.len() == RECENT_KEEP {
                self.recent.pop_front();
            }
            self.recent.push_back(a);
        }
    }

    /// Does the transcript end on a `tool_use` still awaiting its `tool_result`?
    ///
    /// That is a tool running, or the agent blocked on the human: a permission
    /// prompt, a question, a plan approval. Unlike [`derive_status`], this never
    /// decays to [`Status::Idle`] with time — a dialog left open for minutes is
    /// still open, and anything that types into the pane would answer it.
    ///
    /// [`derive_status`]: AgentSession::derive_status
    pub fn awaiting_tool(&self) -> bool {
        match (self.last_kind, &self.last_tail) {
            (Some(Kind::Assistant), Tail::ToolUse(id)) => self.open_tools.iter().any(|t| t == id),
            _ => false,
        }
    }

    /// Derive [`Status`] from the accumulated tail state against `now_ms`.
    ///
    /// Pure and deterministic: it reads only fields already computed by
    /// [`apply`](AgentSession::apply) plus the passed-in `now_ms`; it never
    /// calls the system clock, so the dwell/idle thresholds are testable with
    /// fixed times. `World::refresh` stamps `now` and calls this.
    pub fn derive_status(&self, now_ms: u64) -> Status {
        // Idle wins first: nothing recent enough to claim either way. The
        // activity clock is normally the last transcript timestamp, but for a
        // vendor that doesn't stamp every turn (aider's markdown log carries
        // only a session-start time) that clock freezes and would trap the
        // session in Idle, so we fall back to the file's mtime.
        let activity_ms = if self.vendor.has_line_timestamps() {
            self.last_ts_ms
        } else {
            Some(self.mtime_ms)
        };
        if let Some(ts) = activity_ms {
            if now_ms.saturating_sub(ts) >= IDLE_MS {
                return Status::Idle;
            }
        } else {
            return Status::Idle; // no timestamped activity at all
        }
        match (self.last_kind, &self.last_tail) {
            // Last line is an unresolved trailing tool_use.
            (Some(Kind::Assistant), Tail::ToolUse(id))
                if self.open_tools.iter().any(|t| t == id) =>
            {
                if mode_prompts(self.perm_mode.as_deref()) {
                    let dwelled = self
                        .pending_tool_ts
                        .map(|ts| now_ms.saturating_sub(ts) >= APPROVAL_DWELL_MS)
                        .unwrap_or(false);
                    if dwelled {
                        Status::WaitingApproval
                    } else {
                        Status::Working // still inside the dwell band
                    }
                } else {
                    Status::Working // auto-approving mode: a running tool
                }
            }
            // Assistant turn ended cleanly on a text block.
            (Some(Kind::Assistant), Tail::Text) => Status::WaitingPrompt,
            // Last line is a user prompt or a tool result: the agent is working.
            (Some(Kind::User), _) => Status::Working,
            // Any other assistant tail (resolved tool, empty) → working.
            (Some(Kind::Assistant), _) => Status::Working,
            // `last_kind` is only ever set to User/Assistant (see `apply`);
            // Other never lands here, but the match must be total.
            (Some(Kind::Other), _) => Status::Working,
            (None, _) => Status::Idle,
        }
    }

    /// Mark the session caught up to `len` without reading any bytes. Used for a
    /// bounded cold start ([`World::refresh_since`]): a stale transcript is not
    /// tailed, but recording its length here means a later [`tail`](Self::tail)
    /// reads only genuinely new bytes rather than re-scanning the whole backlog.
    fn mark_caught_up(&mut self, len: u64) {
        self.offset = len;
        self.partial.clear();
        self.skip_line = false;
    }

    fn tail(&mut self) -> std::io::Result<()> {
        let meta = std::fs::metadata(&self.path)?;
        self.tail_from(&meta)
    }

    /// [`tail`](Self::tail) with the transcript's metadata already in hand, so a
    /// refresh that has just stat'ed the file doesn't stat it again.
    fn tail_from(&mut self, meta: &std::fs::Metadata) -> std::io::Result<()> {
        self.mtime_ms = to_ms(meta.modified()?);
        let len = meta.len();
        if len < self.offset {
            self.reset(); // truncated or rewritten: start over
            self.mtime_ms = to_ms(meta.modified()?);
        }
        if len == self.offset {
            return Ok(());
        }
        let mut f = std::fs::File::open(&self.path)?;
        // Bound a single tail: when we are further behind than `TAIL_CAP` (a huge
        // backlog, or the first read of a large live transcript), read only the
        // last `TAIL_CAP` bytes and resync at the next line, never `read_to_end`
        // hundreds of MB in one loop tick.
        let capped = len - self.offset > TAIL_CAP;
        let seek_to = if capped { len - TAIL_CAP } else { self.offset };
        f.seek(SeekFrom::Start(seek_to))?;
        let mut new = Vec::with_capacity((len - seek_to) as usize);
        f.read_to_end(&mut new)?;
        self.offset = len;
        // A capped window begins mid-line: that fragment is unparseable, so drop
        // any stale partial and skip to the first newline, the same way the rest
        // of an overlong line is skipped.
        if capped {
            self.partial.clear();
            self.skip_line = true;
        }
        let mut start = 0;
        if self.skip_line {
            match new.iter().position(|&b| b == b'\n') {
                Some(nl) => {
                    start = nl + 1;
                    self.skip_line = false;
                }
                None => return Ok(()), // still inside the dropped line
            }
        }
        // `partial` is always empty while skipping, so `start` indexes `buf` too.
        let mut buf = std::mem::take(&mut self.partial);
        if buf.is_empty() {
            buf = new;
        } else {
            buf.extend_from_slice(&new);
        }
        while let Some(nl) = buf[start..].iter().position(|&b| b == b'\n') {
            let line = &buf[start..start + nl];
            if let Ok(text) = std::str::from_utf8(line) {
                if let Some(ev) = self.vendor.parse_line(text.trim_end_matches('\r')) {
                    self.apply(ev);
                }
            }
            start += nl + 1;
        }
        // Hold an unfinished line for the next read, unless it has already
        // outgrown the cap: then drop it and resync at its end. Buffering it
        // instead grew `partial` for as long as the line kept growing.
        if (buf.len() - start) as u64 > TAIL_CAP {
            self.skip_line = true;
        } else {
            self.partial = buf[start..].to_vec();
        }
        Ok(())
    }

    fn scan_subagents(&mut self, now_ms: u64) {
        self.subagents_total = 0;
        self.subagents_active = 0;
        let dir = match self.path.parent() {
            Some(p) => p.join(&self.id).join("subagents"),
            None => return,
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "jsonl") {
                self.subagents_total += 1;
                if let Ok(meta) = entry.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if now_ms.saturating_sub(to_ms(modified)) < SUBAGENT_ACTIVE_MS {
                            self.subagents_active += 1;
                        }
                    }
                }
            }
        }
    }
}

/// All sessions under one projects root, sorted most-recent first.
#[derive(Debug)]
pub struct World {
    pub root: PathBuf,
    /// The vendor whose transcripts live under `root`. Every session discovered
    /// here is tagged with it and parsed through its adapter.
    pub vendor: Vendor,
    pub sessions: Vec<AgentSession>,
}

impl World {
    /// A world over a Claude Code projects root: the default vendor. Other
    /// vendors use [`World::for_vendor`] with their own transcript root.
    pub fn new(root: PathBuf) -> World {
        World::for_vendor(root, Vendor::ClaudeCode)
    }

    /// A world over `root` whose transcripts were written by `vendor`. atrium
    /// holds one world per vendor root and merges their sessions for display.
    pub fn for_vendor(root: PathBuf, vendor: Vendor) -> World {
        World {
            root,
            vendor,
            sessions: Vec::new(),
        }
    }

    /// Discover new transcripts, tail changed ones, refresh subagent
    /// counts, derive each session's status, and re-sort. Missing roots and
    /// unreadable files are tolerated; a monitor keeps running.
    pub fn refresh(&mut self) {
        self.refresh_impl(0);
    }

    /// Like [`refresh`](World::refresh), but skips *tailing* any transcript
    /// whose mtime predates `cutoff_ms`: metadata only, no byte reads. atrium
    /// passes its own process start time so a cold first scan never blocks on
    /// sessions that stopped writing before atrium existed (§5 of the design).
    /// Discovery, subagent counts, status, and sort still run for every
    /// session; only the (potentially large) tail read is skipped.
    pub fn refresh_since(&mut self, cutoff_ms: u64) {
        self.refresh_impl(cutoff_ms);
    }

    fn refresh_impl(&mut self, cutoff_ms: u64) {
        let now = now_ms();
        // Discover transcripts with a *bounded recursive* walk rather than a fixed
        // depth. Claude Code nests one level (root/<project>/<session>.jsonl), but
        // other vendors nest deeper (two, three, four levels down), so a hardcoded
        // depth-2 scan silently missed them. `project_dir` becomes the transcript's
        // own parent-directory name: the project label for any layout.
        let ext = self.vendor.transcript_ext();
        // A set, not a scan of `sessions` per path: that was quadratic in the
        // history size (measured ~10 ms a refresh at ~440 sessions).
        let known: std::collections::HashSet<PathBuf> =
            self.sessions.iter().map(|s| s.path.clone()).collect();
        for fpath in scan_transcripts(&self.root, ext, MAX_SCAN_DEPTH) {
            if known.contains(&fpath) {
                continue;
            }
            let id = fpath
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let pname = fpath
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut s = AgentSession::new(self.vendor, fpath, id, pname);
            s.first_seen_ms = now;
            self.sessions.push(s);
        }
        // One metadata call per session decides both whether its transcript still
        // exists (a session whose file can't be stat'ed is dropped, exactly as the
        // old `path.exists()` pass did) and what to tail.
        self.sessions.retain_mut(|s| {
            let Ok(m) = std::fs::metadata(&s.path) else {
                return false;
            };
            if cutoff_ms == 0 {
                let _ = s.tail_from(&m);
            } else {
                // Metadata-only: learn mtime cheaply; tail only if fresh.
                s.mtime_ms = m.modified().map(to_ms).unwrap_or(s.mtime_ms);
                if s.mtime_ms >= cutoff_ms {
                    let _ = s.tail_from(&m);
                } else if s.offset == 0 {
                    // Stale and never read: skip the (possibly huge)
                    // backlog, but mark it caught up so a later refresh()
                    // tails only new bytes instead of re-reading history.
                    s.mark_caught_up(m.len());
                }
            }
            s.scan_subagents(now);
            s.status = s.derive_status(now);
            true
        });
        self.sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime_ms));
    }
}

/// Directory-depth bound for [`scan_transcripts`]: how far below a world's root
/// the discovery walk descends. Claude nests one level; other vendors up to a few,
/// so 6 finds every known layout without risking a runaway walk of an unrelated
/// deep tree.
const MAX_SCAN_DEPTH: usize = 6;

/// Collect transcript files matching `ext` under `root`, descending at most
/// `max_depth` directory levels (iterative, no recursion-depth risk).
/// Best-effort: unreadable directories are skipped. A `subagents/` directory is
/// NOT descended into: a claude session keeps its sub-agent transcripts there and
/// they are counted separately ([`AgentSession::scan_subagents`]), never as
/// top-level sessions.
fn scan_transcripts(root: &std::path::Path, ext: &str, max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            // The listing already carries each entry's type (on Windows it comes
            // with the directory read itself); `Path::is_dir` stat'ed every entry
            // again, and that walk was half of a refresh. A symlink still follows
            // its target, as `is_dir` did.
            let is_dir = match entry.file_type() {
                Ok(t) if t.is_symlink() => p.is_dir(),
                Ok(t) => t.is_dir(),
                Err(_) => p.is_dir(),
            };
            if is_dir {
                let is_subagents = p.file_name().is_some_and(|n| n == "subagents");
                if depth < max_depth && !is_subagents {
                    stack.push((p, depth + 1));
                }
            } else if p.extension().is_some_and(|e| e == ext) {
                found.push(p);
            }
        }
    }
    found
}

pub fn now_ms() -> u64 {
    to_ms(SystemTime::now())
}

fn to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod bound_tests {
    use super::{AgentSession, Vendor, MAX_OPEN_TOOLS, TAIL_CAP};
    use std::io::Write;
    use std::path::PathBuf;

    const USER_PROMPT: &str = r#"{"type":"user","timestamp":"2026-08-28T20:36:40.000Z","message":{"role":"user","content":"go"}}"#;

    fn session(tag: &str) -> (PathBuf, AgentSession) {
        let base = std::env::temp_dir().join(format!("agsess_bound_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("s.jsonl");
        std::fs::write(&path, b"").unwrap();
        let s = AgentSession::new(Vendor::ClaudeCode, path, "s".into(), "p".into());
        (base, s)
    }

    fn append(s: &AgentSession, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn an_unterminated_line_never_grows_partial_past_the_cap() {
        // A line arriving in sub-cap chunks, each tick below TAIL_CAP (so no read
        // is capped), must not accumulate in `partial` without bound: that was a
        // per-session buffer growing as long as the line did.
        let (base, mut s) = session("partial");
        let chunk = vec![b'x'; (TAIL_CAP / 2) as usize];
        for _ in 0..5 {
            append(&s, &chunk);
            s.tail().unwrap();
            assert!(
                s.partial.len() as u64 <= TAIL_CAP,
                "partial {} > cap {TAIL_CAP}",
                s.partial.len()
            );
        }
        // The overlong line is skipped, and parsing resyncs at the next line.
        append(&s, format!("\n{USER_PROMPT}\n").as_bytes());
        s.tail().unwrap();
        assert_eq!(s.user_msgs, 1);
        assert!(s.partial.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_capped_read_without_a_newline_holds_nothing() {
        // Further behind than the cap, with no line boundary in the window: the
        // window is mid-line, so keep none of it rather than a cap-sized partial.
        let (base, mut s) = session("capped");
        append(&s, &vec![b'y'; TAIL_CAP as usize + 16]);
        s.tail().unwrap();
        assert!(s.partial.is_empty());
        append(&s, format!("\n{USER_PROMPT}\n").as_bytes());
        s.tail().unwrap();
        assert_eq!(s.user_msgs, 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn open_tools_is_bounded_and_keeps_the_newest() {
        // Interrupted tool calls never get a tool_result; their ids must not
        // pile up forever. The newest id survives so status still resolves.
        let (base, mut s) = session("tools");
        let mut body = String::new();
        let n = MAX_OPEN_TOOLS + 50;
        for i in 0..n {
            body.push_str(&format!(
                r#"{{"type":"assistant","timestamp":"2026-08-28T20:36:50.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t{i}","name":"Bash","input":{{}}}}]}}}}"#
            ));
            body.push('\n');
        }
        append(&s, body.as_bytes());
        s.tail().unwrap();
        assert!(
            s.open_tools.len() <= MAX_OPEN_TOOLS,
            "{}",
            s.open_tools.len()
        );
        assert!(s.awaiting_tool(), "the newest open tool must be kept");
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod scan_tests {
    use super::{scan_transcripts, MAX_SCAN_DEPTH};

    #[test]
    fn finds_transcripts_at_varying_depths_and_skips_subagents() {
        // A throwaway tree: a claude-style depth-2 transcript with a subagents
        // sibling dir, plus a deeper (depth-4) vendor transcript.
        let base = std::env::temp_dir().join(format!("agsess_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("proj/sessA/subagents")).unwrap();
        std::fs::write(base.join("proj/sessA.jsonl"), b"{}").unwrap();
        std::fs::write(base.join("proj/sessA/subagents/sub.jsonl"), b"{}").unwrap();
        std::fs::create_dir_all(base.join("a/b/c")).unwrap();
        std::fs::write(base.join("a/b/c/deep.jsonl"), b"{}").unwrap();

        let names: Vec<String> = scan_transcripts(&base, "jsonl", MAX_SCAN_DEPTH)
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();

        assert!(
            names.contains(&"sessA.jsonl".to_string()),
            "depth-2: {names:?}"
        );
        assert!(
            names.contains(&"deep.jsonl".to_string()),
            "depth-4: {names:?}"
        );
        assert!(
            !names.contains(&"sub.jsonl".to_string()),
            "subagents excluded: {names:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
