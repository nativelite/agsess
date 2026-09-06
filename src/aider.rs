//! Format adapter for Aider's on-disk chat history files
//! (`<project-cwd>/.aider.chat.history.md`).
//!
//! Unlike Claude Code, aider writes **markdown**, not JSONL. A turn is not a
//! self-contained JSON line; it spans multiple lines: user turns are every
//! consecutive `> `-prefixed line; assistant turns are the bare-text blocks
//! that follow. `parse_line` classifies each line *individually* by its
//! syntactic role. The sessions machinery takes the last meaningful
//! classification as the tail state, which is sufficient for attention-status
//! derivation.
//!
//! # NOTE: RESEARCHED STUB
//!
//! Format derived from the public `aider-chat/aider` repository (stable since
//! ~v0.40) and the official documentation. **Not verified against a live
//! `.aider.chat.history.md` captured by the module author.** The file format is
//! configurable via `--chat-history-file`, so the discovery path may differ
//! between installs. If you have a real transcript, run the tests against it
//! and remove this notice.
//!
//! Known gaps vs the Claude Code adapter:
//! - **No per-message timestamps.** Only the session-start header carries a
//!   timestamp, so `last_ts_ms` is set once and never updated per turn.
//!   `derive_status` will report Idle for sessions whose start timestamp is
//!   older than `IDLE_MS`. The lead may want to fall back to `mtime_ms` for
//!   vendors where `last_ts_ms` is stale.
//! - **No token counts.** Aider's markdown history has no usage data; all
//!   `tokens_in`/`tokens_out` fields are 0.
//! - **No permission_mode.** Aider's approval model differs from Claude Code's;
//!   `permission_mode` is always `None`.
//! - **Discovery mismatch.** `sessions::World` currently scans only `*.jsonl`.
//!   Wiring aider requires the lead to add `.aider.chat.history.md` discovery.
//!
//! # Transcript location
//!
//! Default: `<cwd>/.aider.chat.history.md` (the working directory where aider
//! was launched). Configurable via `--chat-history-file`. No central store:
//! each project has its own file.
//!
//! # Line format (targeted)
//!
//! ```text
//! # aider chat started at 2024-01-15 10:30:00
//!
//! > /add main.py
//!
//! > Add a factorial function.
//!
//! Sure! I'll add a factorial function.
//!
//! main.py
//! <<<<<<< SEARCH
//! =======
//! def factorial(n):
//! >>>>>>> REPLACE
//!
//! > /commit
//!
//! Committing main.py
//! Commit abc1234 main: add factorial function
//!
//! >
//! ```

use crate::claude::{Kind, LineEvent, Tail};

/// Parse one line of an aider chat-history file.
///
/// Returns `None` for lines that carry no status meaning (empty lines,
/// bare `>` prompts, code-fence markers, etc.).
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let line = line.trim_end_matches('\r');
    // Skip empty lines: they are separators in the markdown format.
    if line.trim().is_empty() {
        return None;
    }

    // Session-start header: "# aider chat started at YYYY-MM-DD HH:MM:SS"
    // This is the only line that carries a timestamp.
    if let Some(rest) = line.strip_prefix("# aider chat started at ") {
        let ts_ms = parse_aider_ts(rest.trim());
        return Some(LineEvent {
            kind: Kind::Other,
            ts_ms,
            model: None,
            tokens_in: 0,
            tokens_out: 0,
            action: Some("aider session start".to_string()),
            cwd: None,
            branch: None,
            permission_mode: None,
            tail: Tail::None,
        });
    }

    // "#### aider (model-name)": assistant-turn header, optionally with model.
    // Seen in some aider versions that write structured section headers.
    if let Some(rest) = line.strip_prefix("#### aider") {
        let model = extract_model(rest);
        return Some(LineEvent {
            kind: Kind::Assistant,
            ts_ms: None,
            model,
            tokens_in: 0,
            tokens_out: 0,
            action: None,
            cwd: None,
            branch: None,
            permission_mode: None,
            tail: Tail::Text,
        });
    }

    // Other markdown headers (# …, #### user, ---) are structural noise.
    if line.starts_with('#') || line.starts_with("####") || line == "---" {
        return Some(LineEvent {
            kind: Kind::Other,
            ts_ms: None,
            model: None,
            tokens_in: 0,
            tokens_out: 0,
            action: None,
            cwd: None,
            branch: None,
            permission_mode: None,
            tail: Tail::None,
        });
    }

    // User turn: lines beginning with "> " (commands and messages alike).
    // A bare ">" with nothing after it is an empty prompt: skip it.
    if let Some(msg) = line.strip_prefix("> ") {
        let msg = msg.trim();
        if msg.is_empty() {
            return None;
        }
        return Some(LineEvent {
            kind: Kind::User,
            ts_ms: None,
            model: None,
            tokens_in: 0,
            tokens_out: 0,
            action: Some(format!("> {}", preview(msg))),
            cwd: None,
            branch: None,
            permission_mode: None,
            tail: Tail::None,
        });
    }
    if line == ">" {
        return None;
    }

    // Everything else is an assistant-response line.
    // Code fences, diff markers, and commit lines all fall here; the last
    // non-empty non-special line in a quiet session will be an assistant line,
    // which correctly sets the tail to Text (→ WaitingPrompt).
    Some(LineEvent {
        kind: Kind::Assistant,
        ts_ms: None,
        model: None,
        tokens_in: 0,
        tokens_out: 0,
        action: Some(preview(line)),
        cwd: None,
        branch: None,
        permission_mode: None,
        tail: Tail::Text,
    })
}

/// Extract a model name from the tail of a `#### aider (model)` header line.
/// Returns `None` when no parenthesised name is present.
fn extract_model(rest: &str) -> Option<String> {
    let inner = rest.trim().strip_prefix('(')?.strip_suffix(')')?;
    let s = inner.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Parse `YYYY-MM-DD HH:MM:SS` (aider session-start header format) into epoch
/// milliseconds.  Returns `None` on any malformed input.
fn parse_aider_ts(s: &str) -> Option<u64> {
    // Expected: "2024-01-15 10:30:00"  (19 bytes minimum)
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b' '
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        s.get(range)?.parse::<i64>().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || h > 23
        || mi > 59
        || sec > 60
    {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    if secs < 0 {
        return None;
    }
    Some(secs as u64 * 1000)
}

/// Howard Hinnant's civil-days-since-epoch algorithm (shared with claude.rs).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Truncate to ≤46 characters, collapsing whitespace, for display.
fn preview(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if out.chars().count() >= 46 {
            out.push('…');
            break;
        }
        out.push(if c == '\n' || c == '\r' || c == '\t' {
            ' '
        } else {
            c
        });
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Tests: synthetic transcript lines in the targeted format
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- session start header ---

    #[test]
    fn parses_session_start_timestamp() {
        let ev = parse_line("# aider chat started at 2024-01-15 10:30:00").unwrap();
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.action.as_deref(), Some("aider session start"));
        // 2024-01-15 10:30:00 UTC in epoch ms
        // Days from 1970-01-01 to 2024-01-15 = 19737
        // seconds = 19737 * 86400 + 10*3600 + 30*60 = 1705314600
        assert_eq!(ev.ts_ms, Some(1_705_314_600_000));
    }

    #[test]
    fn session_start_with_trailing_spaces() {
        let ev = parse_line("# aider chat started at 2024-06-01 09:00:00  ").unwrap();
        assert_eq!(ev.kind, Kind::Other);
        assert!(ev.ts_ms.is_some());
    }

    #[test]
    fn non_aider_hash_header_is_other() {
        let ev = parse_line("# some other markdown heading").unwrap();
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tail, Tail::None);
    }

    // --- user turns ---

    #[test]
    fn user_message_kind_and_action() {
        let ev = parse_line("> Tell me what this function does.").unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
        assert_eq!(ev.action.as_deref(), Some("> Tell me what this function does."));
        assert_eq!(ev.ts_ms, None);
        assert_eq!(ev.tokens_in, 0);
    }

    #[test]
    fn user_command_slash() {
        let ev = parse_line("> /add main.py").unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.action.as_deref(), Some("> /add main.py"));
    }

    #[test]
    fn user_commit_command() {
        let ev = parse_line("> /commit").unwrap();
        assert_eq!(ev.kind, Kind::User);
    }

    #[test]
    fn user_long_message_truncated() {
        let long = "> ".to_string() + &"x".repeat(60);
        let ev = parse_line(&long).unwrap();
        assert_eq!(ev.kind, Kind::User);
        let action = ev.action.unwrap();
        assert!(action.ends_with('…'), "should end with ellipsis, got: {action:?}");
        assert!(action.chars().count() <= 50); // "> " + 46 chars + "…"
    }

    #[test]
    fn bare_gt_is_skipped() {
        assert!(parse_line(">").is_none());
    }

    #[test]
    fn gt_space_only_is_skipped() {
        assert!(parse_line(">  ").is_none()); // "> " prefix but empty message
    }

    // --- assistant turns ---

    #[test]
    fn assistant_text_line() {
        let ev = parse_line("Sure! I'll add the factorial function.").unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        assert!(ev.action.is_some());
    }

    #[test]
    fn assistant_commit_line() {
        let ev = parse_line("Commit abc1234 main: add factorial function").unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
    }

    #[test]
    fn assistant_diff_marker_line() {
        let ev = parse_line("<<<<<<< SEARCH").unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
    }

    #[test]
    fn assistant_action_is_preview() {
        let ev = parse_line("Short reply.").unwrap();
        assert_eq!(ev.action.as_deref(), Some("Short reply."));
    }

    // --- aider section headers (structured format) ---

    #[test]
    fn aider_header_without_model() {
        let ev = parse_line("#### aider").unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.model, None);
        assert_eq!(ev.tail, Tail::Text);
    }

    #[test]
    fn aider_header_with_model() {
        let ev = parse_line("#### aider (claude-3-5-sonnet-20241022)").unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.model.as_deref(), Some("claude-3-5-sonnet-20241022"));
        assert_eq!(ev.tail, Tail::Text);
    }

    #[test]
    fn aider_header_with_gpt_model() {
        let ev = parse_line("#### aider (gpt-4o)").unwrap();
        assert_eq!(ev.model.as_deref(), Some("gpt-4o"));
    }

    // --- skip cases ---

    #[test]
    fn empty_line_skipped() {
        assert!(parse_line("").is_none());
    }

    #[test]
    fn whitespace_only_skipped() {
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn separator_line_is_other() {
        let ev = parse_line("---").unwrap();
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tail, Tail::None);
    }

    // --- permission_mode always None ---

    #[test]
    fn permission_mode_always_none() {
        for line in &[
            "> user message",
            "assistant response",
            "# aider chat started at 2024-01-01 00:00:00",
            "#### aider (gpt-4o)",
        ] {
            if let Some(ev) = parse_line(line) {
                assert!(
                    ev.permission_mode.is_none(),
                    "line {line:?} unexpectedly set permission_mode"
                );
            }
        }
    }

    // --- no tokens ---

    #[test]
    fn tokens_always_zero() {
        for line in &["> user", "assistant text", "#### aider (gpt-4)"] {
            if let Some(ev) = parse_line(line) {
                assert_eq!(ev.tokens_in, 0, "line {line:?}");
                assert_eq!(ev.tokens_out, 0, "line {line:?}");
            }
        }
    }

    // --- timestamp parser ---

    #[test]
    fn parse_aider_ts_epoch() {
        // 1970-01-01 00:00:00 → 0 ms
        assert_eq!(parse_aider_ts("1970-01-01 00:00:00"), Some(0));
    }

    #[test]
    fn parse_aider_ts_bad_format_returns_none() {
        assert!(parse_aider_ts("not a date").is_none());
        assert!(parse_aider_ts("2024-13-01 00:00:00").is_none()); // month 13
        assert!(parse_aider_ts("2024-01-15T10:30:00").is_none()); // T separator (JSONL format)
    }

    #[test]
    fn parse_aider_ts_midnight() {
        // 2024-01-01 00:00:00 UTC
        // Days from 1970 to 2024-01-01 = 19723 days
        let ms = parse_aider_ts("2024-01-01 00:00:00").unwrap();
        assert_eq!(ms, 19723 * 86400 * 1000);
    }

    // --- status-derivation integration sketch ---
    //
    // We cannot call `derive_status` directly here (it lives on AgentSession),
    // but we can verify the tail/kind that would drive it for common patterns.

    #[test]
    fn last_user_line_drives_working() {
        // A session ending on a user prompt → kind=User → derive_status=Working
        let ev = parse_line("> Fix the off-by-one error.").unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
        // With last_kind=User, derive_status returns Working.
    }

    #[test]
    fn last_assistant_text_drives_waiting_prompt() {
        // A session ending on an assistant text block → tail=Text → derive_status=WaitingPrompt
        let ev = parse_line("I've fixed the off-by-one error in line 42.").unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        // With last_kind=Assistant + Tail::Text, derive_status returns WaitingPrompt.
    }
}
