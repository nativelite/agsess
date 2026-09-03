//! Format adapter for Goose (block/goose) on-disk session transcripts.
//!
//! Session files are located at:
//!   Linux/macOS:  `~/.local/share/goose/sessions/<session>.jsonl`
//!   Windows:      `%APPDATA%\goose\sessions\<session>.jsonl`
//!
//! Each line is a single JSON message — no outer wrapper, unlike Claude Code's
//! envelope. The target format follows the Anthropic messages schema that Goose
//! is built on:
//! ```text
//! {"role":"user","content":[{"type":"text","text":"<prompt>"}],"created":<unix_s>}
//! {"role":"assistant","content":[{"type":"text","text":"<reply>"},
//!   {"type":"tool_use","id":"<call_id>","name":"<tool>","input":{}}],
//!  "created":<unix_s>}
//! {"role":"user","content":[{"type":"tool_result","tool_use_id":"<call_id>",
//!   "content":[{"type":"text","text":"<result>"}]}],"created":<unix_s>}
//! ```
//!
//! NOTE: This format is **UNVERIFIED against a real Goose transcript**. The
//! target format above was inferred from the Goose project's architecture
//! (github.com/block/goose) and the Anthropic messages API it builds on. Tests
//! cover the implemented behaviour against synthetic fixtures only. Before
//! shipping, obtain a real `~/.local/share/goose/sessions/*.jsonl` sample,
//! diff it against the fixtures below, and remove this NOTE when confirmed.
//!
//! Because Goose does not record per-message token usage or the active model in
//! its session transcripts, `tokens_in`, `tokens_out`, and `model` are always
//! `0` / `None`. `cwd`, `branch`, and `permission_mode` are likewise absent.

use crate::claude::{Kind, LineEvent, Tail};
use json::Value;

/// Parse one Goose transcript line. Returns `None` for non-JSON lines.
///
/// NOTE: Format is **UNVERIFIED** against a real Goose transcript; see module doc.
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let v = json::parse(line).ok()?;
    let kind = match v.get("role").and_then(Value::as_str) {
        Some("user") => Kind::User,
        Some("assistant") => Kind::Assistant,
        _ => Kind::Other,
    };
    // `created` is a Unix timestamp in whole seconds; convert to milliseconds.
    let ts_ms = v
        .get("created")
        .and_then(Value::as_i64)
        .filter(|&n| n > 0)
        .map(|n| (n as u64) * 1000);
    let action = match kind {
        Kind::Assistant => action_assistant(&v),
        Kind::User => action_user(&v),
        Kind::Other => None,
    };
    let tail = match kind {
        Kind::User | Kind::Assistant => tail_of(&v),
        Kind::Other => Tail::None,
    };
    Some(LineEvent {
        kind,
        ts_ms,
        model: None,          // not stored in Goose transcripts
        tokens_in: 0,         // not stored in Goose transcripts
        tokens_out: 0,        // not stored in Goose transcripts
        action,
        cwd: None,            // not stored in Goose transcripts
        branch: None,         // not stored in Goose transcripts
        permission_mode: None, // no equivalent concept in Goose
        tail,
    })
}

/// Derive a display action from an assistant message: last tool_use wins, else
/// a preview of the last text block.
fn action_assistant(v: &Value) -> Option<String> {
    let content = v.get("content")?.as_array()?;
    for block in content.iter().rev() {
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

/// Derive a display action from a user message: a tool_result block is labeled
/// "tool result"; a plain string content or an array of text blocks becomes a
/// prompt preview.
fn action_user(v: &Value) -> Option<String> {
    match v.get("content")? {
        Value::String(s) => Some(format!("> {}", preview(s))),
        Value::Array(blocks) => {
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

/// Classify the final status-relevant content block in a message.
fn tail_of(v: &Value) -> Tail {
    let Some(content) = v.get("content") else {
        return Tail::None;
    };
    let blocks = match content.as_array() {
        Some(b) => b,
        None => return Tail::None, // plain string content (user prompt)
    };
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
            Some("tool_result") => {
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                return Tail::ToolResult(id);
            }
            Some("text") => return Tail::Text,
            _ => continue,
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

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic transcript lines targeting the researched Goose format.
    // NOTE: UNVERIFIED against a real Goose transcript.

    const ASSISTANT_TOOL: &str = r#"{"role":"assistant","content":[{"type":"text","text":"Running the linter."},{"type":"tool_use","id":"call-1","name":"bash","input":{"command":"cargo clippy"}}],"created":1756483200}"#;

    const ASSISTANT_TEXT: &str = r#"{"role":"assistant","content":[{"type":"text","text":"All checks pass."}],"created":1756483210}"#;

    const USER_PROMPT: &str = r#"{"role":"user","content":[{"type":"text","text":"run clippy"}],"created":1756483190}"#;

    const USER_PROMPT_STRING: &str =
        r#"{"role":"user","content":"run clippy","created":1756483190}"#;

    const USER_TOOL_RESULT: &str = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[{"type":"text","text":"warning: unused import"}]}],"created":1756483205}"#;

    #[test]
    fn assistant_with_tool_use() {
        let ev = parse_line(ASSISTANT_TOOL).unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.ts_ms, Some(1_756_483_200_000));
        assert_eq!(ev.action.as_deref(), Some("tool: bash"));
        assert_eq!(ev.tail, Tail::ToolUse("call-1".to_string()));
        assert_eq!(ev.tokens_in, 0);
        assert_eq!(ev.tokens_out, 0);
        assert!(ev.model.is_none());
        assert!(ev.permission_mode.is_none());
        assert!(ev.cwd.is_none());
    }

    #[test]
    fn assistant_text_only() {
        let ev = parse_line(ASSISTANT_TEXT).unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.action.as_deref(), Some("All checks pass."));
        assert_eq!(ev.tail, Tail::Text);
    }

    #[test]
    fn user_prompt_block() {
        let ev = parse_line(USER_PROMPT).unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.action.as_deref(), Some("> ..."));
        // A user text-block produces Tail::Text; status derivation uses
        // (Kind::User, _) => Working regardless, so the tail value is harmless.
        assert_eq!(ev.tail, Tail::Text);
    }

    #[test]
    fn user_prompt_string_content() {
        // Some versions/providers may emit content as a plain string.
        let ev = parse_line(USER_PROMPT_STRING).unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.action.as_deref(), Some("> run clippy"));
        assert_eq!(ev.tail, Tail::None);
    }

    #[test]
    fn user_tool_result() {
        let ev = parse_line(USER_TOOL_RESULT).unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.action.as_deref(), Some("tool result"));
        assert_eq!(ev.tail, Tail::ToolResult("call-1".to_string()));
    }

    #[test]
    fn non_json_and_empty_return_none() {
        assert!(parse_line("not json").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn unknown_role_degrades_to_other() {
        let ev =
            parse_line(r#"{"role":"system","content":[{"type":"text","text":"You are..."}],"created":1}"#)
                .unwrap();
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.action, None);
        assert_eq!(ev.tail, Tail::None);
    }

    #[test]
    fn missing_created_gives_no_timestamp() {
        let ev =
            parse_line(r#"{"role":"assistant","content":[{"type":"text","text":"hi"}]}"#).unwrap();
        assert!(ev.ts_ms.is_none());
    }

    #[test]
    fn created_zero_is_not_a_timestamp() {
        let ev =
            parse_line(r#"{"role":"assistant","content":[{"type":"text","text":"hi"}],"created":0}"#)
                .unwrap();
        assert!(ev.ts_ms.is_none());
    }

    #[test]
    fn text_preview_truncates_long_messages() {
        let long = "a".repeat(100);
        let line = format!(
            r#"{{"role":"assistant","content":[{{"type":"text","text":"{long}"}}],"created":1}}"#
        );
        let ev = parse_line(&line).unwrap();
        let action = ev.action.unwrap();
        assert!(action.ends_with('…'), "expected truncation marker");
        assert!(action.chars().count() <= 47);
    }

    #[test]
    fn multiblock_assistant_last_tool_use_wins() {
        // If assistant sends text *then* a tool_use, action is the tool call.
        let ev = parse_line(ASSISTANT_TOOL).unwrap();
        assert_eq!(ev.action.as_deref(), Some("tool: bash"));
        assert_eq!(ev.tail, Tail::ToolUse("call-1".to_string()));
    }

    #[test]
    fn tail_tool_result_captures_use_id() {
        let ev = parse_line(USER_TOOL_RESULT).unwrap();
        assert_eq!(ev.tail, Tail::ToolResult("call-1".to_string()));
    }
}
