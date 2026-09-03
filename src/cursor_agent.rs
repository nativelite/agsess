//! Format adapter for Cursor IDE agent session transcripts.
//!
//! # NOTE — RESEARCHED STUB (unverified against a real transcript)
//!
//! **What we know (verified via reverse-engineering):**
//!
//! Cursor (cursor.sh) stores data in two places:
//!
//! 1. **Main conversations (SQLite, NOT JSONL):** `%APPDATA%\Cursor\User\globalStorage\state.vscdb`
//!    (Windows), `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` (macOS),
//!    `~/.config/Cursor/User/globalStorage/state.vscdb` (Linux). Table `cursorDiskKV`;
//!    keys like `composerData:{UUID}` and `bubbleId:{composerId}:{bubbleId}`. Individual
//!    message objects carry `type` (1=user, 2=assistant), `text`, `tokenCount`,
//!    `toolResults`, `thinkingDurationMs`, etc.
//!
//! 2. **Background-agent sub-agent transcripts (JSONL exists!):**
//!    `~/.cursor/projects/<project-id>/agent-transcripts/<session-id>/subagents/*.jsonl`.
//!    These exist when Cursor's Background Agent spawns sub-agents. The exact JSONL
//!    schema is **not publicly documented** — format is derived by analogy with the
//!    Anthropic/OpenAI wire format that Cursor uses for API calls.
//!
//! **Target format:** this parser targets a JSONL file where each line is one
//! conversation turn in the OpenAI/Anthropic chat completions wire format (dual-shape
//! support — see examples below). Tool call IDs are stable enough for
//! `Tail::ToolUse`/`Tail::ToolResult` pairing.
//!
//! **Discovery mismatch:** `sessions::World` currently scans `*.jsonl` under the
//! Claude projects root. Wiring Cursor requires the lead to add `~/.cursor/projects/`
//! as a secondary scan root and handle the nested `agent-transcripts/` layout. A board
//! note records this gap.
//!
//! # Targeted line format
//!
//! ```json
//! {"role":"user","content":"Add a hello function","timestamp":"2024-01-15T10:00:00.000Z","conversationId":"abc-123"}
//! {"role":"assistant","content":[{"type":"text","text":"I'll add it now."},{"type":"tool_use","id":"call_01","name":"str_replace_editor","input":{}}],"timestamp":"2024-01-15T10:00:01.000Z","model":"claude-3-5-sonnet-20241022","usage":{"promptTokens":120,"completionTokens":45}}
//! {"role":"tool","tool_call_id":"call_01","content":"File edited.","timestamp":"2024-01-15T10:00:02.000Z"}
//! {"role":"assistant","content":[{"type":"text","text":"Done! The function is in place."}],"timestamp":"2024-01-15T10:00:03.000Z","model":"claude-3-5-sonnet-20241022","usage":{"promptTokens":165,"completionTokens":12}}
//! ```
//!
//! Alternatively, Cursor may use the OpenAI `tool_calls` array instead of
//! Anthropic-style `type:tool_use` blocks. This parser handles both shapes:
//!
//! ```json
//! {"role":"assistant","content":null,"tool_calls":[{"id":"call_02","type":"function","function":{"name":"read_file"}}],"timestamp":"2024-01-15T10:00:04.000Z"}
//! {"role":"tool","tool_call_id":"call_02","content":"...","timestamp":"2024-01-15T10:00:05.000Z"}
//! ```

use crate::claude::{parse_ts, Kind, LineEvent, Tail};
use json::Value;

/// Parse one Cursor agent transcript line. Returns `None` for non-JSON noise.
///
/// Handles two tool-call shapes:
/// - Anthropic blocks: `content[].type == "tool_use"` with an `id` field.
/// - OpenAI `tool_calls` array: `tool_calls[].id` + `tool_calls[].function.name`.
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let v = json::parse(line).ok()?;

    let kind = match v.get("role").and_then(Value::as_str) {
        Some("user") => Kind::User,
        Some("assistant") => Kind::Assistant,
        // "tool" messages (tool results delivered as a separate role) count
        // as user turns for status purposes: the agent is still working.
        Some("tool") => Kind::User,
        _ => Kind::Other,
    };

    let usage = v.get("usage");
    let tok = |key: &str| -> u64 {
        usage
            .and_then(|u| u.get(key))
            .and_then(Value::as_i64)
            .map_or(0, |n| n.max(0) as u64)
    };

    let action = match kind {
        Kind::Assistant => assistant_action(&v),
        Kind::User => user_action(&v),
        Kind::Other => None,
    };

    let tail = match kind {
        Kind::Assistant => assistant_tail(&v),
        Kind::User => user_tail(&v),
        Kind::Other => Tail::None,
    };

    Some(LineEvent {
        kind,
        ts_ms: v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts),
        model: v
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tokens_in: tok("promptTokens"),
        tokens_out: tok("completionTokens"),
        action,
        cwd: None,            // Cursor does not record cwd in transcripts
        branch: None,         // Cursor does not record git branch in transcripts
        permission_mode: None, // no equivalent in Cursor's agent model
        tail,
    })
}

/// "Last relevant block wins" — a `tool_use` block reads as `tool: Name`;
/// otherwise a text preview. Handles both Anthropic blocks and OpenAI
/// `tool_calls` arrays.
fn assistant_action(v: &Value) -> Option<String> {
    // OpenAI tool_calls shape
    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
        if !calls.is_empty() {
            let last = calls.last().unwrap();
            let name = last
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            return Some(format!("tool: {name}"));
        }
    }
    // Anthropic content blocks shape (or plain string content)
    match v.get("content") {
        Some(Value::Array(blocks)) => {
            for block in blocks.iter().rev() {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                        return Some(format!("tool: {name}"));
                    }
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        return Some(preview(text));
                    }
                    _ => continue,
                }
            }
            None
        }
        Some(Value::String(s)) if !s.is_empty() => Some(preview(s)),
        _ => None,
    }
}

fn user_action(v: &Value) -> Option<String> {
    // tool-role message (tool result): the caller mapped it to Kind::User
    if v.get("role").and_then(Value::as_str) == Some("tool") {
        return Some("tool result".to_string());
    }
    match v.get("content") {
        Some(Value::String(s)) => Some(format!("> {}", preview(s))),
        Some(Value::Array(blocks)) => {
            let has_result = blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
            Some(if has_result {
                "tool result".to_string()
            } else {
                "> ...".to_string()
            })
        }
        _ => None,
    }
}

/// Classify the final relevant block of an assistant turn for status tracking.
///
/// Priority order (same as `claude::tail_of`):
/// 1. OpenAI `tool_calls` array — last entry's `id` → `Tail::ToolUse`.
/// 2. Anthropic `content[].type == "tool_use"` — last such block's `id` → `Tail::ToolUse`.
/// 3. Trailing `text` block → `Tail::Text`.
/// 4. Anything else → `Tail::None`.
fn assistant_tail(v: &Value) -> Tail {
    // OpenAI tool_calls shape
    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
        if let Some(last) = calls.last() {
            let id = last
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Tail::ToolUse(id);
        }
    }
    // Anthropic blocks shape
    if let Some(blocks) = v.get("content").and_then(Value::as_array) {
        for block in blocks.iter().rev() {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    return Tail::ToolUse(id);
                }
                Some("text") => return Tail::Text,
                _ => continue,
            }
        }
    }
    // Plain string content (rare on assistant turns)
    if matches!(v.get("content"), Some(Value::String(_))) {
        return Tail::Text;
    }
    Tail::None
}

/// Classify a user-role turn for status tracking.
///
/// - A `tool`-role message carries `tool_call_id` → `Tail::ToolResult`.
/// - A user message with `tool_result` blocks → `Tail::ToolResult`.
/// - A plain user prompt → `Tail::None`.
fn user_tail(v: &Value) -> Tail {
    // tool-role message: "tool_call_id" resolves the open call
    if v.get("role").and_then(Value::as_str) == Some("tool") {
        let id = v
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Tail::ToolResult(id);
    }
    // Anthropic-style tool_result blocks inside user content
    if let Some(blocks) = v.get("content").and_then(Value::as_array) {
        for block in blocks.iter().rev() {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                return Tail::ToolResult(id);
            }
        }
    }
    Tail::None
}

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
// Tests — synthetic transcript lines in the targeted format
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> LineEvent {
        parse_line(line).expect("should parse")
    }

    // ── noise / non-JSON ──────────────────────────────────────────────────

    #[test]
    fn non_json_returns_none() {
        assert!(parse_line("not json").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn unknown_role_is_other() {
        let ev = parse(r#"{"role":"system","content":"be helpful"}"#);
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tail, Tail::None);
        assert!(ev.action.is_none());
    }

    // ── user turns ────────────────────────────────────────────────────────

    #[test]
    fn plain_user_text() {
        let line = r#"{"role":"user","content":"Add a hello function","timestamp":"2024-01-15T10:00:00.000Z"}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.action.as_deref(), Some("> Add a hello function"));
        assert_eq!(ev.tail, Tail::None);
        assert!(ev.ts_ms.is_some());
    }

    #[test]
    fn user_text_no_timestamp() {
        let ev = parse(r#"{"role":"user","content":"hello"}"#);
        assert_eq!(ev.kind, Kind::User);
        assert!(ev.ts_ms.is_none());
        assert_eq!(ev.tail, Tail::None);
    }

    #[test]
    fn user_text_preview_truncated() {
        let long = "a".repeat(60);
        let line = format!(r#"{{"role":"user","content":"{long}"}}"#);
        let ev = parse(&line);
        let action = ev.action.unwrap();
        assert!(action.ends_with('…'), "expected ellipsis, got: {action}");
        assert!(action.chars().count() <= 49);
    }

    // ── assistant text turns ──────────────────────────────────────────────

    #[test]
    fn assistant_text_block() {
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"Done! The function is in place."}],"timestamp":"2024-01-15T10:00:03.000Z","model":"claude-3-5-sonnet-20241022","usage":{"promptTokens":165,"completionTokens":12}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.model.as_deref(), Some("claude-3-5-sonnet-20241022"));
        assert_eq!(ev.tokens_in, 165);
        assert_eq!(ev.tokens_out, 12);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(
            ev.action.as_deref(),
            Some("Done! The function is in place.")
        );
    }

    #[test]
    fn assistant_plain_string_content() {
        // Some Cursor versions may emit plain string for simple assistant replies
        let line = r#"{"role":"assistant","content":"Looks good!","timestamp":"2024-01-15T10:00:01.000Z"}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("Looks good!"));
    }

    // ── assistant tool calls — Anthropic blocks shape ─────────────────────

    #[test]
    fn anthropic_tool_use_block() {
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"I'll add it now."},{"type":"tool_use","id":"call_01","name":"str_replace_editor","input":{}}],"timestamp":"2024-01-15T10:00:01.000Z","model":"claude-3-5-sonnet-20241022","usage":{"promptTokens":120,"completionTokens":45}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::ToolUse("call_01".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: str_replace_editor"));
        assert_eq!(ev.tokens_in, 120);
        assert_eq!(ev.tokens_out, 45);
    }

    #[test]
    fn anthropic_text_after_tool_use_wins() {
        // Last block wins — text after tool_use → Tail::Text
        let line = r#"{"role":"assistant","content":[{"type":"tool_use","id":"call_x","name":"read_file","input":{}},{"type":"text","text":"Here is the result."}]}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("Here is the result."));
    }

    #[test]
    fn anthropic_tool_use_after_text_wins() {
        // Last block wins — tool_use after text → Tail::ToolUse
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"Running:"},{"type":"tool_use","id":"call_y","name":"bash","input":{}}]}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::ToolUse("call_y".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: bash"));
    }

    // ── assistant tool calls — OpenAI tool_calls shape ────────────────────

    #[test]
    fn openai_tool_calls_array() {
        let line = r#"{"role":"assistant","content":null,"tool_calls":[{"id":"call_02","type":"function","function":{"name":"read_file","arguments":"{}"}}],"timestamp":"2024-01-15T10:00:04.000Z"}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::ToolUse("call_02".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: read_file"));
    }

    #[test]
    fn openai_multiple_tool_calls_last_wins() {
        let line = r#"{"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"read_file","arguments":"{}"}},{"id":"call_b","type":"function","function":{"name":"write_file","arguments":"{}"}}]}"#;
        let ev = parse(line);
        // last tool_call in array is call_b
        assert_eq!(ev.tail, Tail::ToolUse("call_b".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: write_file"));
    }

    // ── tool-role messages (tool results, OpenAI shape) ───────────────────

    #[test]
    fn tool_role_message() {
        let line = r#"{"role":"tool","tool_call_id":"call_01","content":"File edited successfully.","timestamp":"2024-01-15T10:00:02.000Z"}"#;
        let ev = parse(line);
        // tool-role maps to Kind::User (agent still working)
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::ToolResult("call_01".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool result"));
    }

    #[test]
    fn tool_role_empty_tool_call_id() {
        let line = r#"{"role":"tool","content":"ok"}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        // empty id still produces ToolResult (id resolution degrades gracefully)
        assert_eq!(ev.tail, Tail::ToolResult(String::new()));
    }

    // ── user messages with Anthropic tool_result blocks ───────────────────

    #[test]
    fn user_anthropic_tool_result_block() {
        let line = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_03","content":"done"}],"timestamp":"2024-01-15T10:00:05.000Z"}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::ToolResult("call_03".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool result"));
    }

    // ── token accounting ──────────────────────────────────────────────────

    #[test]
    fn tokens_default_zero_when_absent() {
        let ev = parse(r#"{"role":"assistant","content":[{"type":"text","text":"hi"}]}"#);
        assert_eq!(ev.tokens_in, 0);
        assert_eq!(ev.tokens_out, 0);
    }

    #[test]
    fn tokens_parsed_from_usage() {
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"promptTokens":100,"completionTokens":50}}"#;
        let ev = parse(line);
        assert_eq!(ev.tokens_in, 100);
        assert_eq!(ev.tokens_out, 50);
    }

    // ── fields always absent for Cursor ──────────────────────────────────

    #[test]
    fn cwd_branch_permission_mode_always_none() {
        let ev = parse(r#"{"role":"user","content":"test"}"#);
        assert!(ev.cwd.is_none());
        assert!(ev.branch.is_none());
        assert!(ev.permission_mode.is_none());
    }

    // ── timestamp parsing ─────────────────────────────────────────────────

    #[test]
    fn timestamp_with_millis() {
        let line = r#"{"role":"user","content":"hi","timestamp":"2024-06-01T12:00:00.500Z"}"#;
        let ev = parse(line);
        assert!(ev.ts_ms.is_some());
        assert_eq!(ev.ts_ms.unwrap() % 1000, 500);
    }

    #[test]
    fn timestamp_without_millis() {
        let line = r#"{"role":"user","content":"hi","timestamp":"2024-01-01T00:00:00Z"}"#;
        let ev = parse(line);
        assert!(ev.ts_ms.is_some());
    }

    #[test]
    fn malformed_timestamp_becomes_none() {
        let line = r#"{"role":"user","content":"hi","timestamp":"not-a-date"}"#;
        let ev = parse(line);
        assert!(ev.ts_ms.is_none());
    }

    // ── empty / degenerate inputs ─────────────────────────────────────────

    #[test]
    fn empty_content_array() {
        let ev = parse(r#"{"role":"assistant","content":[]}"#);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::None);
        assert!(ev.action.is_none());
    }

    #[test]
    fn missing_content_field() {
        let ev = parse(r#"{"role":"user"}"#);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
    }

    // ── status-derivation integration sketches ────────────────────────────

    #[test]
    fn last_user_turn_drives_working() {
        // Kind::User → derive_status returns Working
        let ev = parse(r#"{"role":"user","content":"Fix it"}"#);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
    }

    #[test]
    fn tool_result_drives_working() {
        // tool result is Kind::User → derive_status returns Working
        let ev = parse(r#"{"role":"tool","tool_call_id":"call_x","content":"ok"}"#);
        assert_eq!(ev.kind, Kind::User);
        assert!(matches!(ev.tail, Tail::ToolResult(_)));
    }

    #[test]
    fn assistant_text_drives_waiting_prompt() {
        // Kind::Assistant + Tail::Text → derive_status returns WaitingPrompt
        let ev =
            parse(r#"{"role":"assistant","content":[{"type":"text","text":"All done!"}]}"#);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
    }

    #[test]
    fn unresolved_tool_use_drives_working_or_waiting_approval() {
        // Kind::Assistant + Tail::ToolUse → Working or WaitingApproval (dwell-gated)
        let ev = parse(r#"{"role":"assistant","content":[{"type":"tool_use","id":"call_z","name":"bash","input":{}}]}"#);
        assert_eq!(ev.kind, Kind::Assistant);
        assert!(matches!(ev.tail, Tail::ToolUse(_)));
    }
}
