//! Format adapter for OpenCode's session messages.
//!
//! **OpenCode** (<https://opencode.ai>, <https://github.com/sst/opencode>) is an
//! open-source terminal AI coding assistant. It stores sessions in a **SQLite**
//! database (v1.2.0+) at:
//!
//! | Platform | Path |
//! |----------|------|
//! | Linux    | `~/.local/share/opencode/opencode.db` (or `$XDG_DATA_HOME/opencode/`) |
//! | macOS    | `~/Library/Application Support/opencode/opencode.db` |
//! | Windows  | `%APPDATA%\opencode\opencode.db` |
//!
//! Pre-v1.2.0 versions used flat JSON files under
//! `~/.local/share/opencode/storage/session/{hash}/{id}.json` and
//! `~/.local/share/opencode/storage/message/{sessionID}/msg_{msgID}.json`.
//!
//! The `message` table stores rows with (approximately):
//! ```text
//! id TEXT, sessionID TEXT, role TEXT,
//! timestamp INTEGER (Unix ms),
//! model JSON {providerID, modelID},
//! tokens JSON {input, output, reasoning, cache {read, write}},
//! finish TEXT, error TEXT, parentID TEXT
//! ```
//!
//! # Target JSONL export format
//!
//! `parse_line` targets a **JSONL export** where each line is one message row
//! serialised from the SQLite database. A future integration could emit this by
//! `SELECT`-ing the `message` table and writing rows as JSON lines. The
//! `opencode run --format json` streaming output uses a coarser event schema
//! (`step_start` / `tool_use` / `text` / `step_finish`) that is intentionally
//! *not* the target here: the per-message granularity is what `Status` needs.
//!
//! Example line:
//! ```json
//! {
//!   "id": "01JK5...",
//!   "sessionID": "ses_01JK4...",
//!   "role": "assistant",
//!   "timestamp": 1700000000000,
//!   "model": {"providerID": "anthropic", "modelID": "claude-3-5-sonnet-20241022"},
//!   "tokens": {"input": 1000, "output": 200, "reasoning": 0,
//!               "cache": {"read": 800, "write": 0}},
//!   "finish": "stop",
//!   "content": [{"type": "text", "text": "..."}]
//! }
//! ```
//!
//! # NOTE
//! This format is a **researched-stub**. The field names and structure are
//! derived from the sst/opencode source code, database schema, and secondary
//! sources (DeepWiki, community docs), and have **not been verified against a
//! real running OpenCode instance or a real exported transcript**. If the actual
//! export format differs, update the tests (the only contract) and this header.
//!
//! Content blocks in the `content` array are assumed to follow the standard
//! Anthropic multi-block convention (`type`/`id`/`tool_use_id`) because
//! OpenCode normalises its internal representation to that shape across
//! providers — but this too is unverified from a real transcript.

use json::Value;

// Re-export the shared types so callers only import this module.
pub use crate::claude::{Kind, LineEvent, Tail};

/// Parse one OpenCode JSONL export line. Returns `None` for non-JSON lines —
/// a monitor treats those as noise, not an error.
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let v = json::parse(line).ok()?;

    let kind = match v.get("role").and_then(Value::as_str) {
        Some("user") => Kind::User,
        Some("assistant") => Kind::Assistant,
        _ => Kind::Other,
    };

    // `timestamp` is a Unix millisecond integer on the message row.
    let ts_ms = v
        .get("timestamp")
        .and_then(Value::as_i64)
        .filter(|&n| n > 0)
        .map(|n| n as u64);

    // `model` is an object {providerID, modelID}; we join them as "provider/model".
    let model = {
        let m = v.get("model");
        let provider = m
            .and_then(|m| m.get("providerID"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = m
            .and_then(|m| m.get("modelID"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match (provider, id) {
            ("", "") => None,
            ("", id) => Some(id.to_string()),
            (p, "") => Some(p.to_string()),
            (p, id) => Some(format!("{p}/{id}")),
        }
    };

    // `tokens` is a nested object. Cache-write counts as billable input.
    let tokens = v.get("tokens");
    let tok_i64 = |path: &[&str]| -> u64 {
        let mut cur = tokens;
        for &key in path {
            cur = cur.and_then(|n| n.get(key));
        }
        cur.and_then(Value::as_i64).map_or(0, |n| n.max(0) as u64)
    };
    let tokens_in = tok_i64(&["input"]) + tok_i64(&["cache", "write"]);
    let tokens_out = tok_i64(&["output"]);

    let action = match kind {
        Kind::Assistant => v.get("content").and_then(assistant_action),
        Kind::User => v.get("content").and_then(user_action),
        Kind::Other => None,
    };

    let tail = match kind {
        Kind::User | Kind::Assistant => v.get("content").map(tail_of).unwrap_or_default(),
        Kind::Other => Tail::None,
    };

    Some(LineEvent {
        kind,
        ts_ms,
        model,
        tokens_in,
        tokens_out,
        action,
        // OpenCode does not embed cwd/branch in individual messages;
        // those are session-level fields in the `session` table.
        cwd: None,
        branch: None,
        // OpenCode has no equivalent to Claude Code's permissionMode.
        permission_mode: None,
        tail,
    })
}

/// Derive the display action string from an assistant message's content array.
/// Last `tool_use` wins ("tool: Name"); otherwise last `text` block is previewed.
fn assistant_action(content: &Value) -> Option<String> {
    let blocks = content.as_array()?;
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

/// Derive the display action string from a user message's content.
fn user_action(content: &Value) -> Option<String> {
    match content {
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

/// Classify the final relevant content block for `Status` derivation.
fn tail_of(content: &Value) -> Tail {
    let blocks = match content.as_array() {
        Some(b) => b,
        None => return Tail::None,
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

    // ── non-JSON / malformed lines ────────────────────────────────────────────

    #[test]
    fn non_json_returns_none() {
        assert!(parse_line("not json at all").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn json_without_role_is_other() {
        let ev = parse_line(r#"{"id":"x","model":{"providerID":"anthropic","modelID":"claude-3"}}"#).unwrap();
        assert_eq!(ev.kind, Kind::Other);
    }

    // ── timestamp ─────────────────────────────────────────────────────────────

    #[test]
    fn timestamp_from_integer_ms() {
        let line = r#"{"role":"user","timestamp":1700000000000,"content":"hello"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.ts_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn missing_timestamp_gives_none_ts() {
        let line = r#"{"role":"user","content":"hello"}"#;
        let ev = parse_line(line).unwrap();
        assert!(ev.ts_ms.is_none());
    }

    #[test]
    fn zero_timestamp_gives_none_ts() {
        // 0 ms is treated as absent (sentinel for "not set").
        let line = r#"{"role":"user","timestamp":0,"content":"hello"}"#;
        let ev = parse_line(line).unwrap();
        assert!(ev.ts_ms.is_none());
    }

    // ── role / kind ───────────────────────────────────────────────────────────

    #[test]
    fn user_role_maps_to_user_kind() {
        let line = r#"{"role":"user","timestamp":1000,"content":"hi"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::User);
    }

    #[test]
    fn assistant_role_maps_to_assistant_kind() {
        let line = r#"{"role":"assistant","timestamp":2000,"content":[{"type":"text","text":"Hello!"}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::Assistant);
    }

    #[test]
    fn system_role_maps_to_other() {
        let line = r#"{"role":"system","content":[{"type":"text","text":"You are helpful."}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::Other);
    }

    // ── model ─────────────────────────────────────────────────────────────────

    #[test]
    fn model_joined_from_provider_and_id() {
        let line = r#"{"role":"assistant","model":{"providerID":"anthropic","modelID":"claude-3-5-sonnet-20241022"},"content":[]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(
            ev.model.as_deref(),
            Some("anthropic/claude-3-5-sonnet-20241022")
        );
    }

    #[test]
    fn model_id_only_when_provider_absent() {
        let line = r#"{"role":"assistant","model":{"modelID":"gpt-4o"},"content":[]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn missing_model_is_none() {
        let line = r#"{"role":"user","content":"hello"}"#;
        let ev = parse_line(line).unwrap();
        assert!(ev.model.is_none());
    }

    // ── tokens ────────────────────────────────────────────────────────────────

    #[test]
    fn tokens_parsed_from_nested_object() {
        let line = r#"{"role":"assistant","tokens":{"input":150,"output":42,"reasoning":0,"cache":{"read":800,"write":0}},"content":[]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tokens_in, 150);
        assert_eq!(ev.tokens_out, 42);
    }

    #[test]
    fn cache_write_added_to_tokens_in() {
        // Cache-write bytes are billable input; add them to tokens_in.
        let line = r#"{"role":"assistant","tokens":{"input":100,"output":20,"cache":{"read":0,"write":50}},"content":[]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tokens_in, 150); // 100 + 50
        assert_eq!(ev.tokens_out, 20);
    }

    #[test]
    fn missing_tokens_default_to_zero() {
        let line = r#"{"role":"user","content":"hello"}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tokens_in, 0);
        assert_eq!(ev.tokens_out, 0);
    }

    // ── absent cwd / branch / permission_mode ─────────────────────────────────

    #[test]
    fn opencode_has_no_cwd_branch_or_permission_mode() {
        let line = r#"{"role":"user","content":"hello"}"#;
        let ev = parse_line(line).unwrap();
        assert!(ev.cwd.is_none());
        assert!(ev.branch.is_none());
        assert!(ev.permission_mode.is_none());
    }

    // ── action strings ────────────────────────────────────────────────────────

    #[test]
    fn assistant_text_action_preview() {
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"Sure, I can help with that."}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.action.as_deref(), Some("Sure, I can help with that."));
    }

    #[test]
    fn assistant_tool_use_action() {
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"Let me read that."},{"type":"tool_use","id":"tu_001","name":"read_file","input":{"path":"foo.rs"}}]}"#;
        let ev = parse_line(line).unwrap();
        // Last block is tool_use → action names the tool.
        assert_eq!(ev.action.as_deref(), Some("tool: read_file"));
    }

    #[test]
    fn user_string_content_action() {
        let line = r#"{"role":"user","content":"What does this function do?"}"#;
        let ev = parse_line(line).unwrap();
        assert!(ev.action.as_deref().unwrap_or("").starts_with("> "));
    }

    #[test]
    fn user_tool_result_action() {
        let line = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_001","content":"file contents here"}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.action.as_deref(), Some("tool result"));
    }

    // ── tail / status ─────────────────────────────────────────────────────────

    #[test]
    fn tail_text_on_clean_assistant_turn() {
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"Done."}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tail, Tail::Text);
    }

    #[test]
    fn tail_tool_use_carries_id() {
        let line = r#"{"role":"assistant","content":[{"type":"tool_use","id":"tu_abc","name":"bash","input":{"cmd":"ls"}}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tail, Tail::ToolUse("tu_abc".into()));
    }

    #[test]
    fn tail_tool_result_carries_tool_use_id() {
        let line = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_abc","content":"ok"}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tail, Tail::ToolResult("tu_abc".into()));
    }

    #[test]
    fn tail_none_on_other_kind() {
        let line = r#"{"role":"system","content":[{"type":"text","text":"You are helpful."}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tail, Tail::None);
    }

    #[test]
    fn tail_none_on_empty_content_array() {
        let line = r#"{"role":"assistant","content":[]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tail, Tail::None);
    }

    // ── last-block-wins ordering ──────────────────────────────────────────────

    #[test]
    fn last_block_wins_for_tail() {
        // Two tool_use blocks: the second id should win.
        let line = r#"{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"read","input":{}},{"type":"tool_use","id":"tu_2","name":"write","input":{}}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tail, Tail::ToolUse("tu_2".into()));
    }

    #[test]
    fn text_after_tool_use_gives_text_tail() {
        // text block follows tool_use → text tail (last block wins).
        let line = r#"{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"bash","input":{}},{"type":"text","text":"All done."}]}"#;
        let ev = parse_line(line).unwrap();
        assert_eq!(ev.tail, Tail::Text);
    }

    // ── action preview truncation ─────────────────────────────────────────────

    #[test]
    fn long_text_action_is_truncated() {
        let long = "a".repeat(100);
        let line = format!(
            r#"{{"role":"assistant","content":[{{"type":"text","text":"{long}"}}]}}"#
        );
        let ev = parse_line(&line).unwrap();
        let action = ev.action.unwrap();
        assert!(action.contains('…'));
        assert!(action.chars().count() <= 48); // 46 content + '…'
    }
}
