//! Integration tests for agsess: the vendor-format adapter (fixtures shaped
//! from real recorded transcript lines, sanitized, no real conversation text),
//! timestamp parsing vectors, incremental tailing against real temp files
//! (including a line split across two appends and a truncation reset), and the
//! attention-status derivation exercised against synthetic fixtures with fixed
//! `now_ms` values so the dwell/idle thresholds are deterministic.

use agsess::claude::{parse_line, parse_ts, Kind, Tail};
use agsess::sessions::{World, APPROVAL_DWELL_MS, IDLE_MS};
use agsess::{Status, Vendor};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

// --- adapter ---------------------------------------------------------------

/// Shaped from a real assistant line: usage + tool_use content block.
const ASSISTANT_TOOL: &str = r#"{"type":"assistant","timestamp":"2026-08-28T20:36:50.248Z","cwd":"D:\\projects\\nativelite","gitBranch":"main","sessionId":"s1","message":{"model":"claude-fable-5","role":"assistant","usage":{"input_tokens":23897,"cache_creation_input_tokens":29847,"cache_read_input_tokens":0,"output_tokens":109},"content":[{"type":"text","text":"Running the check."},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#;

const ASSISTANT_TEXT: &str = r#"{"type":"assistant","timestamp":"2026-08-28T20:37:00.000Z","cwd":"D:\\projects\\nativelite","message":{"model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":20},"content":[{"type":"text","text":"All 42 tests\npass."}]}}"#;

const USER_PROMPT: &str = r#"{"type":"user","timestamp":"2026-08-28T20:36:47.971Z","cwd":"D:\\projects\\nativelite","gitBranch":"main","message":{"role":"user","content":"run the tests"}}"#;

const USER_TOOL_RESULT: &str = r#"{"type":"user","timestamp":"2026-08-28T20:36:52.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;

const MODE_LINE: &str = r#"{"type":"mode","mode":"normal","sessionId":"s1"}"#;

#[test]
fn assistant_line_with_tool_use() {
    let ev = parse_line(ASSISTANT_TOOL).unwrap();
    assert_eq!(ev.kind, Kind::Assistant);
    assert_eq!(ev.model.as_deref(), Some("claude-fable-5"));
    assert_eq!(ev.tokens_in, 23897 + 29847);
    assert_eq!(ev.tokens_out, 109);
    assert_eq!(ev.action.as_deref(), Some("tool: Bash"));
    assert_eq!(ev.cwd.as_deref(), Some("D:\\projects\\nativelite"));
    assert_eq!(ev.branch.as_deref(), Some("main"));
    assert_eq!(ev.ts_ms, parse_ts("2026-08-28T20:36:50.248Z"));
    // extension: the tail is the trailing tool_use, keyed by its id.
    assert_eq!(ev.tail, Tail::ToolUse("t1".to_string()));
}

#[test]
fn assistant_line_with_text_only() {
    let ev = parse_line(ASSISTANT_TEXT).unwrap();
    assert_eq!(ev.action.as_deref(), Some("All 42 tests pass."));
    assert_eq!(ev.tokens_in, 10);
    assert_eq!(ev.tokens_out, 20);
    assert_eq!(ev.tail, Tail::Text);
}

#[test]
fn user_lines() {
    let ev = parse_line(USER_PROMPT).unwrap();
    assert_eq!(ev.kind, Kind::User);
    assert_eq!(ev.action.as_deref(), Some("> run the tests"));
    assert_eq!(ev.tokens_in, 0);
    assert_eq!(ev.tail, Tail::None); // a plain string prompt is not a block

    let ev = parse_line(USER_TOOL_RESULT).unwrap();
    assert_eq!(ev.action.as_deref(), Some("tool result"));
    assert_eq!(ev.tail, Tail::ToolResult("t1".to_string()));
}

#[test]
fn permission_mode_is_read_from_both_shapes() {
    // dedicated permission-mode record
    let pm = parse_line(r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s"}"#)
        .unwrap();
    assert_eq!(pm.kind, Kind::Other);
    assert_eq!(pm.permission_mode.as_deref(), Some("default"));
    // inline on a user record (top level)
    let u = parse_line(
        r#"{"type":"user","timestamp":"2026-08-29T12:00:00.000Z","permissionMode":"acceptEdits","message":{"role":"user","content":"hi"}}"#,
    )
    .unwrap();
    assert_eq!(u.permission_mode.as_deref(), Some("acceptEdits"));
}

#[test]
fn unknown_and_malformed_lines_degrade_gracefully() {
    let ev = parse_line(MODE_LINE).unwrap();
    assert_eq!(ev.kind, Kind::Other);
    assert_eq!(ev.action, None);
    assert_eq!(ev.tail, Tail::None);
    assert_eq!(parse_line("not json at all"), None);
    assert_eq!(parse_line(""), None);
}

#[test]
fn timestamp_vectors() {
    assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_ts("1970-01-02T00:00:00Z"), Some(86_400_000));
    assert_eq!(parse_ts("2000-01-01T00:00:00Z"), Some(946_684_800_000));
    assert_eq!(parse_ts("2020-01-01T00:00:00Z"), Some(1_577_836_800_000));
    assert_eq!(parse_ts("2020-01-01T00:00:00.5Z"), Some(1_577_836_800_500));
    assert_eq!(
        parse_ts("2020-01-01T01:02:03.045Z"),
        Some(1_577_836_800_000 + 3_723_045)
    );
    assert_eq!(parse_ts("garbage"), None);
    assert_eq!(parse_ts("2020-13-01T00:00:00Z"), None);
    assert_eq!(parse_ts("2020-01-01 00:00:00"), None);
}

// --- discovery + tailing ---------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!(
            "agsess-test-{tag}-{}-{}",
            std::process::id(),
            unique()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn write_session(root: &Path, project: &str, id: &str, lines: &[&str]) -> PathBuf {
    let dir = root.join(project);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    let mut f = File::create(&path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
    path
}

#[test]
fn world_discovers_and_aggregates() {
    let td = TempDir::new("agg");
    write_session(
        td.path(),
        "D--projects-nativelite",
        "abc12345",
        &[
            USER_PROMPT,
            ASSISTANT_TOOL,
            USER_TOOL_RESULT,
            ASSISTANT_TEXT,
            MODE_LINE,
        ],
    );
    let mut w = World::new(td.path().to_path_buf());
    w.refresh();
    assert_eq!(w.sessions.len(), 1);
    let s = &w.sessions[0];
    assert_eq!(s.vendor, Vendor::ClaudeCode);
    assert_eq!(s.id, "abc12345");
    assert_eq!(s.user_msgs, 2);
    assert_eq!(s.assistant_msgs, 2);
    assert_eq!(s.tokens_in, 23897 + 29847 + 10);
    assert_eq!(s.tokens_out, 109 + 20);
    assert_eq!(s.model.as_deref(), Some("claude-fable-5"));
    assert_eq!(s.project_name(), "nativelite"); // from cwd, not the munged dir
    assert_eq!(s.last_action, "All 42 tests pass.");
    assert_eq!(s.recent.len(), 4);
    assert!(s.first_seen_ms > 0);
}

#[test]
fn tailing_reads_only_appended_bytes_and_survives_split_lines() {
    let td = TempDir::new("tail");
    let path = write_session(td.path(), "p", "s1", &[USER_PROMPT]);
    let mut w = World::new(td.path().to_path_buf());
    w.refresh();
    assert_eq!(w.sessions[0].user_msgs, 1);

    // Append an assistant line in two pieces, split mid-JSON.
    let (a, b) = ASSISTANT_TOOL.split_at(50);
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(a.as_bytes()).unwrap();
    f.flush().unwrap();
    w.refresh();
    assert_eq!(w.sessions[0].assistant_msgs, 0); // half a line is not a line
    f.write_all(b.as_bytes()).unwrap();
    f.write_all(b"\n").unwrap();
    f.flush().unwrap();
    w.refresh();
    assert_eq!(w.sessions[0].assistant_msgs, 1);
    assert_eq!(w.sessions[0].last_action, "tool: Bash");
}

#[test]
fn truncated_file_resets_aggregates() {
    let td = TempDir::new("trunc");
    let path = write_session(td.path(), "p", "s1", &[USER_PROMPT, ASSISTANT_TEXT]);
    let mut w = World::new(td.path().to_path_buf());
    w.refresh();
    assert_eq!(w.sessions[0].assistant_msgs, 1);

    fs::write(&path, format!("{USER_PROMPT}\n")).unwrap(); // shorter rewrite
    w.refresh();
    let s = &w.sessions[0];
    assert_eq!((s.user_msgs, s.assistant_msgs), (1, 0));
}

#[test]
fn subagents_are_counted() {
    let td = TempDir::new("sub");
    let path = write_session(td.path(), "p", "s1", &[USER_PROMPT]);
    let subdir = path.parent().unwrap().join("s1").join("subagents");
    fs::create_dir_all(&subdir).unwrap();
    fs::write(subdir.join("agent-1.jsonl"), "{}\n").unwrap();
    fs::write(subdir.join("agent-2.jsonl"), "{}\n").unwrap();
    fs::write(subdir.join("notes.txt"), "not a transcript").unwrap();
    let mut w = World::new(td.path().to_path_buf());
    w.refresh();
    assert_eq!(w.sessions[0].subagents_total, 2);
    assert_eq!(w.sessions[0].subagents_active, 2); // just written -> active
}

// --- status derivation (§2.4) ----------------------------------------------
//
// Each fixture models one row of the derivation table. We copy the fixture
// into a temp projects root, tail it through `World::refresh` (so the tail
// state is built exactly as production builds it), then call the *pure*
// `derive_status(now_ms)` with fixed times to exercise the dwell/idle
// thresholds deterministically; no system clock in the assertion path.

/// Load a repo fixture into a fresh temp root and return the tailed session,
/// wrapped so the temp dir outlives the borrow.
fn tail_fixture(name: &str) -> (TempDir, World) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let body = fs::read_to_string(&src).unwrap_or_else(|e| panic!("read {name}: {e}"));
    let td = TempDir::new("status");
    let dir = td.path().join("proj");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("sess.jsonl"), body).unwrap();
    let mut w = World::new(td.path().to_path_buf());
    w.refresh();
    assert_eq!(w.sessions.len(), 1, "fixture {name} produced no session");
    (td, w)
}

/// The last transcript timestamp in every status fixture (2026-08-29T12:00:0X).
/// `now` values below are computed relative to it.
const BASE_TS: u64 = 1_788_004_801_000; // parse_ts("2026-08-29T12:00:01.000Z")

#[test]
fn base_ts_matches_fixtures() {
    // Guard: keep BASE_TS honest against the parser.
    assert_eq!(parse_ts("2026-08-29T12:00:01.000Z"), Some(BASE_TS));
    assert_eq!(parse_ts("2026-08-29T12:00:00.000Z"), Some(BASE_TS - 1000));
    assert_eq!(parse_ts("2026-08-29T12:00:02.000Z"), Some(BASE_TS + 1000));
}

#[test]
fn status_pending_approval_default_dwells_then_escalates() {
    let (_td, w) = tail_fixture("pending-approval-default.jsonl");
    let s = &w.sessions[0];
    // tool_use at BASE_TS, default mode, unresolved.
    // Before the dwell: still Working.
    let before = BASE_TS + APPROVAL_DWELL_MS - 1;
    assert_eq!(s.derive_status(before), Status::Working);
    // After the dwell (but within IDLE_MS): WaitingApproval.
    let after = BASE_TS + APPROVAL_DWELL_MS + 1;
    assert!(
        after - BASE_TS < IDLE_MS,
        "test must stay inside idle window"
    );
    assert_eq!(s.derive_status(after), Status::WaitingApproval);
}

#[test]
fn status_unresolved_auto_stays_working() {
    let (_td, w) = tail_fixture("unresolved-auto.jsonl");
    let s = &w.sessions[0];
    // auto mode: an unresolved tool_use never escalates, even long past dwell.
    let after = BASE_TS + APPROVAL_DWELL_MS + 5_000;
    assert!(after - BASE_TS < IDLE_MS);
    assert_eq!(s.derive_status(after), Status::Working);
}

#[test]
fn status_assistant_text_end_is_waiting_prompt() {
    let (_td, w) = tail_fixture("assistant-text-end.jsonl");
    let s = &w.sessions[0];
    let now = BASE_TS + 2_000;
    assert_eq!(s.derive_status(now), Status::WaitingPrompt);
}

#[test]
fn status_user_last_is_working() {
    let (_td, w) = tail_fixture("user-last.jsonl");
    let s = &w.sessions[0];
    // last line is a user tool_result (at BASE_TS+1s); the agent is working.
    let now = BASE_TS + 2_000;
    assert_eq!(s.derive_status(now), Status::Working);
}

#[test]
fn status_stale_is_idle() {
    let (_td, w) = tail_fixture("stale-idle.jsonl");
    let s = &w.sessions[0];
    // now well past IDLE_MS from the last timestamp.
    let now = BASE_TS + IDLE_MS + 10_000;
    assert_eq!(s.derive_status(now), Status::Idle);
    // and just inside the window it is not idle (assistant text -> WaitingPrompt).
    let fresh = BASE_TS + IDLE_MS - 1_000;
    assert_eq!(s.derive_status(fresh), Status::WaitingPrompt);
}

#[test]
fn refresh_since_skips_tailing_stale_files_but_still_discovers() {
    let td = TempDir::new("since");
    write_session(td.path(), "p", "s1", &[USER_PROMPT, ASSISTANT_TEXT]);
    let mut w = World::new(td.path().to_path_buf());
    // Cutoff in the far future: every file predates it, so none are tailed,
    // the session is discovered (metadata only) but not aggregated.
    w.refresh_since(u64::MAX);
    assert_eq!(w.sessions.len(), 1);
    let s = &w.sessions[0];
    assert_eq!(s.assistant_msgs, 0, "stale file must not be tailed");
    assert!(s.mtime_ms > 0, "metadata (mtime) is still learned");
    // A stale skip marks the session caught up to the current length, so a later
    // refresh() does NOT re-read the (possibly huge) backlog; this is the fix
    // for the fleet-scale hang: the first poll after a cold start must not full-
    // read every historical transcript.
    w.refresh();
    assert_eq!(w.sessions[0].assistant_msgs, 0, "stale backlog is not re-read after catch-up");
    // But genuinely new bytes appended after catch-up are still tailed.
    let path = td.path().join("p").join("s1.jsonl");
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f, "{ASSISTANT_TEXT}").unwrap();
    drop(f);
    w.refresh();
    assert_eq!(w.sessions[0].assistant_msgs, 1, "new activity after catch-up is tailed");
}

#[test]
fn tail_caps_a_huge_backlog_and_still_reads_the_recent_tail() {
    // A transcript far larger than the tail cap must not be read in full: tail
    // reads only the last window, resyncs past the mid-file cut, and still
    // aggregates the recent line at the end.
    let td = TempDir::new("cap");
    let dir = td.path().join("p");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.jsonl");
    {
        let mut f = File::create(&path).unwrap();
        // Non-JSON filler (ignored by the parser) to pad past the 16 MiB cap,
        // then a real assistant line at the very end.
        let filler = format!("{}\n", "x".repeat(8192));
        let target = 17 * 1024 * 1024u64; // > TAIL_CAP
        let mut written = 0u64;
        while written < target {
            f.write_all(filler.as_bytes()).unwrap();
            written += filler.len() as u64;
        }
        writeln!(f, "{ASSISTANT_TEXT}").unwrap();
    }
    let mut w = World::new(td.path().to_path_buf());
    w.refresh(); // cutoff 0 -> tail(), capped to the last 16 MiB
    assert_eq!(w.sessions.len(), 1);
    assert_eq!(
        w.sessions[0].assistant_msgs, 1,
        "the recent tail is still aggregated despite the cap"
    );
    // Offset advanced to the full length, so a second refresh is a no-op read.
    w.refresh();
    assert_eq!(w.sessions[0].assistant_msgs, 1, "no re-read after catch-up");
}

// --- multi-vendor dispatch (the lead-wired integration seam) ---------------
//
// These exercise the enum/dispatch wiring end-to-end through `World`, not just
// each vendor's `parse_line` in isolation: a vendor transcript is discovered,
// tailed, dispatched to the *right* adapter, and its status derived.

/// gemini-cli lines, in the format `src/gemini.rs` targets. Routed through the
/// Claude adapter these parse as noise (Kind::Other), so the counts below prove
/// the dispatch, not a hardcoded default, is what makes them legible.
const GEMINI_USER: &str =
    r#"{"role":"user","parts":[{"text":"hello gemini"}],"timestamp":"2024-01-15T10:00:00.000Z"}"#;
const GEMINI_MODEL_TEXT: &str = r#"{"role":"model","parts":[{"text":"Hi!"}],"timestamp":"2024-01-15T10:00:01.000Z","model":"gemini-2.5-pro","usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":20}}"#;

#[test]
fn world_for_vendor_dispatches_to_the_matching_parser() {
    let td = TempDir::new("gemini");
    write_session(td.path(), "proj", "g1", &[GEMINI_USER, GEMINI_MODEL_TEXT]);

    // Same bytes under the default (Claude) vendor are unreadable; this is the
    // control that proves dispatch, not discovery, does the work.
    let mut claude_world = World::new(td.path().to_path_buf());
    claude_world.refresh();
    let cs = &claude_world.sessions[0];
    assert_eq!(
        (cs.user_msgs, cs.assistant_msgs),
        (0, 0),
        "gemini lines are not Claude lines"
    );

    let mut w = World::for_vendor(td.path().to_path_buf(), Vendor::Gemini);
    w.refresh();
    assert_eq!(w.sessions.len(), 1);
    let s = &w.sessions[0];
    assert_eq!(s.vendor, Vendor::Gemini);
    assert_eq!((s.user_msgs, s.assistant_msgs), (1, 1));
    assert_eq!(s.model.as_deref(), Some("gemini-2.5-pro"));
    // A model turn ending on text, read fresh, is a prompt-wait.
    let now = s.last_ts_ms.unwrap() + 1_000;
    assert_eq!(s.derive_status(now), Status::WaitingPrompt);
}

/// Aider writes a markdown chat log (`.md`), and its turns carry no per-line
/// timestamp, so `last_ts_ms` stays `None`. This covers both the `.md`
/// discovery and the idle-trap fix: `derive_status` must fall back to the
/// file's mtime instead of pinning every aider session at `Idle`.
#[test]
fn aider_md_is_discovered_and_escapes_the_idle_trap() {
    let td = TempDir::new("aider");
    let dir = td.path().join("proj");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("s1.md"),
        "> Tell me what this function does.\n\nIt reverses the string in place.\n",
    )
    .unwrap();

    let mut w = World::for_vendor(td.path().to_path_buf(), Vendor::Aider);
    w.refresh();
    assert_eq!(w.sessions.len(), 1, ".md transcript discovered");
    let s = &w.sessions[0];
    assert_eq!(s.vendor, Vendor::Aider);
    assert!(s.user_msgs >= 1, "aider parser dispatched, not Claude's");
    assert_eq!(s.last_ts_ms, None, "aider carries no per-line timestamps");
    // The file was just written, so despite the absent timestamps it is fresh:
    // the mtime fallback keeps it out of Idle and lets the text tail surface.
    let now = agsess::sessions::now_ms();
    assert_ne!(s.derive_status(now), Status::Idle, "mtime fallback, not the idle trap");
    assert_eq!(s.derive_status(now), Status::WaitingPrompt);
}
