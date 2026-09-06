//! Format adapter for Qwen Code's on-disk session transcripts.
//!
//! Qwen Code (<https://github.com/QwenLM/qwen-code>) is a Gemini-CLI-derived
//! multi-backend coding agent. Session transcripts are stored as JSONL at
//! `~/.qwen/projects/<sanitized-cwd>/chats/<session>.jsonl`.
//!
//! ## Line format
//!
//! Each line is a JSON object. Researched fields:
//!
//! | Field       | Values / Notes                                 | Status     |
//! |-------------|------------------------------------------------|------------|
//! | `role`      | `"user"` or `"model"`                          | verified   |
//! | `content`   | `String` (plain prompt) or typed-block array   | verified   |
//! | `timestamp` | ISO 8601, e.g. `"2025-01-15T10:30:00.000Z"`   | verified   |
//! | `cwd`       | working-directory path string                  | verified   |
//! | `gitBranch` | branch name                                    | verified   |
//! | `model`     | model identifier (not always present)          | researched |
//! | tool blocks | `tool_use` / `tool_result` Anthropic-style     | unverified |
//!
//! ## NOTE: tool-call block shape is unverified
//!
//! Qwen Code supports multiple backends (Anthropic, OpenAI, Gemini, Qwen,
//! Ollama). The tool-call block shape in the JSONL may differ by backend. This
//! parser targets the Anthropic-style convention (`type: "tool_use"` with `id`
//! and `name`; `type: "tool_result"` with `tool_use_id`) because Qwen Code's
//! content-block handling is closest to the Anthropic SDK. This has **not been
//! validated against a captured real transcript**; update `parse_line` and the
//! tests if a real transcript reveals a different shape.

use crate::claude::{Kind, LineEvent, Tail};
use json::Value;

/// Parse one Qwen Code transcript line. Returns `None` for non-JSON or empty
/// lines. Unknown roles and malformed content degrade to `Kind::Other` /
/// `Tail::None`: the monitor never crashes on unexpected input.
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let v = json::parse(line).ok()?;

    let kind = match v.get("role").and_then(Value::as_str) {
        Some("user") => Kind::User,
        Some("model") => Kind::Assistant,
        _ => Kind::Other,
    };

    let content = v.get("content");
    let (action, tail) = match kind {
        Kind::User => content_user(content),
        Kind::Assistant => content_model(content),
        Kind::Other => (None, Tail::None),
    };

    Some(LineEvent {
        kind,
        ts_ms: v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::claude::parse_ts),
        model: v
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        // Qwen Code does not embed token counts in the per-line JSONL record.
        tokens_in: 0,
        tokens_out: 0,
        action,
        cwd: v.get("cwd").and_then(Value::as_str).map(str::to_string),
        branch: v
            .get("gitBranch")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        // Qwen Code has no permission-mode concept; always None.
        permission_mode: None,
        tail,
    })
}

/// Decode a model (assistant) turn's `content` field into `(action, tail)`.
fn content_model(content: Option<&Value>) -> (Option<String>, Tail) {
    match content {
        Some(Value::String(s)) => (Some(preview(s)), Tail::Text),
        Some(v) => match v.as_array() {
            Some(blocks) => (model_action(blocks), tail_of_blocks(blocks)),
            None => (None, Tail::None),
        },
        None => (None, Tail::None),
    }
}

/// Decode a user turn's `content` field into `(action, tail)`.
fn content_user(content: Option<&Value>) -> (Option<String>, Tail) {
    match content {
        Some(Value::String(s)) => (Some(format!("> {}", preview(s))), Tail::None),
        Some(v) => match v.as_array() {
            Some(blocks) => (user_action(blocks), tail_of_blocks(blocks)),
            None => (None, Tail::None),
        },
        None => (None, Tail::None),
    }
}

/// Display string for a model turn. Last `tool_use` block wins; falls back
/// to the last `text` block preview.
fn model_action(blocks: &[Value]) -> Option<String> {
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

/// Display string for a user turn: `"tool result"` when a `tool_result` block
/// is present; `"> ..."` otherwise (mirrors `claude.rs` user_action).
fn user_action(blocks: &[Value]) -> Option<String> {
    let has_result = blocks
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
    Some(if has_result {
        "tool result".to_string()
    } else {
        "> ...".to_string()
    })
}

/// Classify the final relevant block for `Status` derivation. Mirrors
/// `claude.rs`'s `tail_of` using the same Anthropic-style type names.
fn tail_of_blocks(blocks: &[Value]) -> Tail {
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
    use crate::claude::{Kind, Tail};

    // ── basic sanity ──────────────────────────────────────────────────────────

    #[test]
    fn non_json_returns_none() {
        assert_eq!(parse_line("not json at all"), None);
        assert_eq!(parse_line(""), None);
    }

    #[test]
    fn unknown_role_is_other() {
        let line = r#"{"role":"system","content":"boot"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tail, Tail::None);
        assert!(ev.action.is_none());
    }

    // ── user turns ───────────────────────────────────────────────────────────

    #[test]
    fn user_plain_string_prompt() {
        let line = r#"{"role":"user","content":"explain this code","timestamp":"2025-06-01T12:00:00.000Z","cwd":"/home/dev/proj","gitBranch":"main"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
        assert_eq!(ev.action.as_deref(), Some("> explain this code"));
        assert!(ev.ts_ms.is_some());
        assert_eq!(ev.cwd.as_deref(), Some("/home/dev/proj"));
        assert_eq!(ev.branch.as_deref(), Some("main"));
    }

    #[test]
    fn user_text_block_array() {
        // Array content with a text block, no tool involvement.
        let line = r#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("> ..."));
    }

    #[test]
    fn user_tool_result_block() {
        let line = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_abc","content":"fn main() {}"}],"timestamp":"2025-06-01T12:00:03.000Z"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::ToolResult("tu_abc".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool result"));
    }

    // ── model turns ──────────────────────────────────────────────────────────

    #[test]
    fn model_plain_string_content() {
        // Some backends may emit content as a plain string.
        let line = r#"{"role":"model","content":"Here is the answer.","timestamp":"2025-06-01T12:00:01.000Z"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("Here is the answer."));
    }

    #[test]
    fn model_text_block() {
        let line = r#"{"role":"model","content":[{"type":"text","text":"Here is the answer."}],"timestamp":"2025-06-01T12:00:01.000Z"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("Here is the answer."));
    }

    #[test]
    fn model_tool_use_block() {
        let line = r#"{"role":"model","content":[{"type":"tool_use","id":"tu_abc","name":"read_file","input":{"path":"/src/main.rs"}}],"timestamp":"2025-06-01T12:00:02.000Z"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::ToolUse("tu_abc".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: read_file"));
    }

    #[test]
    fn model_mixed_text_then_tool_use() {
        // When text precedes a tool_use, the last tool_use wins for both
        // action and tail (last-block-wins walk in reverse).
        let line = r#"{"role":"model","content":[{"type":"text","text":"Let me check."},{"type":"tool_use","id":"tu_xyz","name":"list_dir","input":{}}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tail, Tail::ToolUse("tu_xyz".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: list_dir"));
    }

    // ── metadata fields ───────────────────────────────────────────────────────

    #[test]
    fn model_name_extracted() {
        let line = r#"{"role":"model","content":[{"type":"text","text":"hi"}],"model":"qwen-coder-plus"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.model.as_deref(), Some("qwen-coder-plus"));
    }

    #[test]
    fn permission_mode_always_none() {
        let line = r#"{"role":"user","content":"hi"}"#;
        let ev = parse_line(line).unwrap();
        assert!(ev.permission_mode.is_none());
    }

    #[test]
    fn tokens_always_zero() {
        let line = r#"{"role":"model","content":[{"type":"text","text":"ok"}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tokens_in, 0);
        assert_eq!(ev.tokens_out, 0);
    }

    #[test]
    fn timestamp_parsed_to_epoch_ms() {
        // 2025-01-15T10:30:00.500Z: verify it lands after Nov 2023 epoch baseline.
        let line = r#"{"role":"user","content":"hi","timestamp":"2025-01-15T10:30:00.500Z"}"#;
        let ev = parse_line(line).unwrap();
        assert!(ev.ts_ms.unwrap() > 1_700_000_000_000);
    }

    // ── preview truncation ────────────────────────────────────────────────────

    #[test]
    fn preview_truncates_long_text() {
        let long_text = "a".repeat(60);
        let line = format!(
            r#"{{"role":"model","content":[{{"type":"text","text":"{}"}}]}}"#,
            long_text
        );
        let ev = parse_line(&line).unwrap();
        let action = ev.action.unwrap();
        assert!(action.ends_with('…'), "expected ellipsis, got: {action:?}");
        // 46 visible chars + ellipsis = at most 47 chars
        assert!(action.chars().count() <= 47);
    }
}
