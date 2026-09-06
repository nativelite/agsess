//! Format adapter for GitHub Copilot agent session transcripts.
//!
//! # IMPORTANT: UNVERIFIED FORMAT (researched-stub)
//!
//! GitHub Copilot does not publish a stable local session-transcript format as
//! of the knowledge cutoff for this implementation. Specifically:
//!
//! - **`gh copilot` CLI** (`gh extension install github/gh-copilot`): the
//!   `suggest` and `explain` subcommands are interactive; they do not write
//!   JSONL session files.
//! - **Copilot Chat in VS Code**: chat history is persisted in a SQLite
//!   database at
//!   `%APPDATA%\Code\User\globalStorage\github.copilot-chat\copilot-chat.db`
//!   (Windows) / `~/.config/Code/User/globalStorage/github.copilot-chat/`
//!   (Linux) / `~/Library/Application Support/Code/User/globalStorage/github.copilot-chat/`
//!   (macOS), not JSONL.
//! - **Copilot Coding Agent**: runs in GitHub Actions or via the GitHub web
//!   UI; no canonical local transcript path is documented.
//!
//! This module therefore targets a **documented target format**: a plausible
//! JSONL schema modelled on GitHub Copilot's OpenAI-compatible
//! message/tool-call convention that a future local Copilot CLI *could* emit.
//! All tests are written against synthetic lines in this format.
//!
//! ## Hypothetical transcript location
//!
//! ```text
//! ~/.config/github-copilot/sessions/<session-id>.jsonl          (Linux/macOS)
//! %APPDATA%\GitHub Copilot\sessions\<session-id>.jsonl          (Windows)
//! ```
//!
//! ## Target line format
//!
//! Each line is a self-contained JSON object.  The `type` field classifies it:
//!
//! | `type`        | Meaning                                        |
//! |---------------|------------------------------------------------|
//! | `user`        | A human prompt turn                           |
//! | `assistant`   | A model reply (text, no tool call)             |
//! | `tool_call`   | The model requested a tool; `id` tracks it     |
//! | `tool_result` | Result fed back; `tool_call_id` resolves `id`  |
//! | anything else | Ignored → `Kind::Other`                        |
//!
//! Example transcript:
//!
//! ```jsonl
//! {"timestamp":"2024-01-15T10:30:45.123Z","type":"user","content":"Fix the auth bug","session_id":"s1"}
//! {"timestamp":"2024-01-15T10:30:46.456Z","type":"assistant","content":"I'll look at the auth module.","model":"gpt-4o","session_id":"s1"}
//! {"timestamp":"2024-01-15T10:30:47.789Z","type":"tool_call","name":"read_file","id":"call_abc","session_id":"s1"}
//! {"timestamp":"2024-01-15T10:30:48.000Z","type":"tool_result","tool_call_id":"call_abc","content":"// auth.rs …","session_id":"s1"}
//! {"timestamp":"2024-01-15T10:30:49.000Z","type":"assistant","content":"The issue is on line 42.","model":"gpt-4o","session_id":"s1"}
//! ```
//!
//! Optional fields present on any line: `cwd`, `branch`.
//! Token counts are not surfaced in the target format; `tokens_in`/`tokens_out`
//! are always 0. `permission_mode` has no Copilot equivalent; it is always
//! `None`.
//!
// NOTE: This format has NOT been verified against a real GitHub Copilot
// transcript. It is a researched-stub. Update this module (format, location,
// and tests) once a real Copilot session file can be inspected.

use crate::claude::{parse_ts, Kind, LineEvent, Tail};
use json::Value;

/// Parse one Copilot session transcript line.
///
/// Returns `None` for non-JSON lines (treated as noise by the monitor).
/// Unknown `type` values degrade to `Kind::Other`/`Tail::None`: never a crash.
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let v = json::parse(line).ok()?;

    let ts_ms = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);

    let model = v
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
    let branch = v
        .get("branch")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let record_type = v.get("type").and_then(Value::as_str).unwrap_or("");

    let (kind, tail, action) = match record_type {
        "user" => {
            let content = v.get("content").and_then(Value::as_str).unwrap_or("");
            let action = if !content.is_empty() {
                Some(format!("> {}", preview(content)))
            } else {
                None
            };
            (Kind::User, Tail::None, action)
        }
        "assistant" => {
            let content = v.get("content").and_then(Value::as_str).unwrap_or("");
            let action = if !content.is_empty() {
                Some(preview(content))
            } else {
                None
            };
            // An assistant turn that is pure text signals WaitingPrompt.
            (Kind::Assistant, Tail::Text, action)
        }
        "tool_call" => {
            let name = v.get("name").and_then(Value::as_str).unwrap_or("?");
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let action = Some(format!("tool: {name}"));
            (Kind::Assistant, Tail::ToolUse(id), action)
        }
        "tool_result" => {
            let tool_call_id = v
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (Kind::User, Tail::ToolResult(tool_call_id), Some("tool result".to_string()))
        }
        _ => (Kind::Other, Tail::None, None),
    };

    Some(LineEvent {
        kind,
        ts_ms,
        model,
        // Copilot transcripts (in the target format) do not surface token counts.
        tokens_in: 0,
        tokens_out: 0,
        action,
        cwd,
        branch,
        // Copilot has no agsess permission-mode concept.
        permission_mode: None,
        tail,
    })
}

fn preview(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if out.chars().count() >= 46 {
            out.push('…');
            break;
        }
        out.push(if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c });
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn ts() -> &'static str {
        "2024-01-15T10:30:45.123Z"
    }

    fn line(fields: &str) -> String {
        format!(r#"{{"timestamp":"{ts}",{fields}}}"#, ts = ts())
    }

    // ── non-JSON noise ────────────────────────────────────────────────────────

    #[test]
    fn non_json_is_none() {
        assert!(parse_line("not json at all").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }

    // ── user turn ────────────────────────────────────────────────────────────

    #[test]
    fn user_turn_basic() {
        let ev = parse_line(&line(r#""type":"user","content":"Fix the auth bug""#)).unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
        assert_eq!(ev.action.as_deref(), Some("> Fix the auth bug"));
        assert!(ev.ts_ms.is_some());
    }

    #[test]
    fn user_turn_empty_content() {
        let ev = parse_line(&line(r#""type":"user","content":"""#)).unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert!(ev.action.is_none());
    }

    // ── assistant turn ────────────────────────────────────────────────────────

    #[test]
    fn assistant_text_turn() {
        let ev = parse_line(&line(
            r#""type":"assistant","content":"The issue is on line 42.","model":"gpt-4o""#,
        ))
        .unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("The issue is on line 42."));
        assert_eq!(ev.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn assistant_long_content_is_previewed() {
        let long = "a".repeat(60);
        let ev = parse_line(&line(&format!(r#""type":"assistant","content":"{long}""#))).unwrap();
        let action = ev.action.unwrap();
        assert!(action.ends_with('…'), "expected trailing ellipsis, got: {action:?}");
        assert!(action.chars().count() <= 47);
    }

    #[test]
    fn assistant_no_model() {
        let ev =
            parse_line(&line(r#""type":"assistant","content":"Hello""#)).unwrap();
        assert!(ev.model.is_none());
    }

    // ── tool_call ─────────────────────────────────────────────────────────────

    #[test]
    fn tool_call_with_id() {
        let ev = parse_line(&line(
            r#""type":"tool_call","name":"read_file","id":"call_abc123""#,
        ))
        .unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::ToolUse("call_abc123".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: read_file"));
    }

    #[test]
    fn tool_call_missing_id_defaults_to_empty() {
        let ev = parse_line(&line(r#""type":"tool_call","name":"list_files""#)).unwrap();
        assert_eq!(ev.tail, Tail::ToolUse(String::new()));
    }

    #[test]
    fn tool_call_missing_name_defaults_to_question_mark() {
        let ev = parse_line(&line(r#""type":"tool_call","id":"call_x""#)).unwrap();
        assert_eq!(ev.action.as_deref(), Some("tool: ?"));
    }

    // ── tool_result ───────────────────────────────────────────────────────────

    #[test]
    fn tool_result_resolves_call_id() {
        let ev = parse_line(&line(
            r#""type":"tool_result","tool_call_id":"call_abc123","content":"ok""#,
        ))
        .unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::ToolResult("call_abc123".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool result"));
    }

    #[test]
    fn tool_result_missing_tool_call_id_defaults_to_empty() {
        let ev = parse_line(&line(r#""type":"tool_result","content":"output""#)).unwrap();
        assert_eq!(ev.tail, Tail::ToolResult(String::new()));
    }

    // ── unknown / system types ────────────────────────────────────────────────

    #[test]
    fn unknown_type_is_other() {
        let ev = parse_line(&line(r#""type":"system","content":"session start""#)).unwrap();
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tail, Tail::None);
        assert!(ev.action.is_none());
    }

    #[test]
    fn missing_type_is_other() {
        let ev = parse_line(&line(r#""content":"no type field""#)).unwrap();
        assert_eq!(ev.kind, Kind::Other);
    }

    // ── optional fields ───────────────────────────────────────────────────────

    #[test]
    fn optional_cwd_and_branch() {
        let ev = parse_line(&line(
            r#""type":"user","content":"hi","cwd":"/home/user/proj","branch":"main""#,
        ))
        .unwrap();
        assert_eq!(ev.cwd.as_deref(), Some("/home/user/proj"));
        assert_eq!(ev.branch.as_deref(), Some("main"));
    }

    #[test]
    fn tokens_always_zero() {
        let ev =
            parse_line(&line(r#""type":"assistant","content":"answer""#)).unwrap();
        assert_eq!(ev.tokens_in, 0);
        assert_eq!(ev.tokens_out, 0);
    }

    #[test]
    fn permission_mode_always_none() {
        let ev =
            parse_line(&line(r#""type":"user","content":"q","permission_mode":"ask""#)).unwrap();
        assert!(ev.permission_mode.is_none());
    }

    // ── timestamp parsing ─────────────────────────────────────────────────────

    #[test]
    fn timestamp_is_parsed() {
        let ev = parse_line(&line(r#""type":"user","content":"x""#)).unwrap();
        // 2024-01-15T10:30:45.123Z → some positive ms value
        assert!(ev.ts_ms.unwrap_or(0) > 0);
    }

    #[test]
    fn missing_timestamp_is_none() {
        let ev = parse_line(r#"{"type":"user","content":"x"}"#).unwrap();
        assert!(ev.ts_ms.is_none());
    }

    // ── round-trip scenario: tool_call → tool_result pair ────────────────────

    #[test]
    fn tool_call_result_pair_tail_ids_match() {
        let call = parse_line(
            r#"{"timestamp":"2024-01-15T10:30:47Z","type":"tool_call","name":"run_cmd","id":"call_xyz"}"#,
        )
        .unwrap();
        let result = parse_line(
            r#"{"timestamp":"2024-01-15T10:30:48Z","type":"tool_result","tool_call_id":"call_xyz","content":"exit 0"}"#,
        )
        .unwrap();
        assert_eq!(call.tail, Tail::ToolUse("call_xyz".to_string()));
        assert_eq!(result.tail, Tail::ToolResult("call_xyz".to_string()));
    }
}
