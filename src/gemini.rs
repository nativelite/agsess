//! Format adapter for Google Gemini CLI session transcripts.
//!
//! **Transcript location:** `~/.gemini/tmp/<session-id>/<session>.jsonl`
//! (one JSON object per line, appended as the conversation progresses).
//!
//! **Format target**: Gemini CLI (`@google/gemini-cli`) records turns in the
//! Gemini API wire format: each line is a JSON object with a `role` field
//! (`"user"` | `"model"`) and a `parts` array. Tool calls appear as
//! `functionCall` parts; tool results as `functionResponse` parts. Optional
//! top-level fields include `timestamp`, `model`, and `usageMetadata`.
//!
//! Example lines (synthetic; field names verified against the Gemini API
//! reference; the exact JSONL layout is derived from the open-source
//! `google-gemini/gemini-cli` TypeScript source):
//!
//! ```json
//! {"role":"user","parts":[{"text":"ls the project"}],"timestamp":"2024-01-15T10:00:00.000Z"}
//! {"role":"model","parts":[{"functionCall":{"name":"bash","args":{"cmd":"ls"},"id":"fc_1"}}],"timestamp":"2024-01-15T10:00:01.000Z","model":"gemini-2.5-pro","usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":8}}
//! {"role":"user","parts":[{"functionResponse":{"name":"bash","id":"fc_1","response":{"output":"src/"}}}],"timestamp":"2024-01-15T10:00:02.000Z"}
//! {"role":"model","parts":[{"text":"Done: only one source dir."}],"timestamp":"2024-01-15T10:00:03.000Z"}
//! ```
//!
//! // NOTE: This parser is a **researched stub**: the field layout was derived
//! // from the Gemini API specification and the open-source gemini-cli TypeScript
//! // source, but has NOT been verified against a real on-disk transcript.
//! // A real run may differ in field names (e.g. `timestamp` key, `id` presence
//! // in `functionCall`, `usageMetadata` structure). Update and re-verify before
//! // marking this `verified`.

use crate::claude::{parse_ts, Kind, LineEvent, Tail};
use json::Value;

/// Parse one Gemini CLI transcript line. Returns `None` for non-JSON noise
/// (same contract as [`crate::claude::parse_line`]).
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let v = json::parse(line).ok()?;

    let kind = match v.get("role").and_then(Value::as_str) {
        Some("user") => Kind::User,
        Some("model") => Kind::Assistant,
        _ => Kind::Other,
    };

    let parts = v.get("parts").and_then(Value::as_array);

    let usage = v.get("usageMetadata");
    let tok = |key: &str| -> u64 {
        usage
            .and_then(|u| u.get(key))
            .and_then(Value::as_i64)
            .map_or(0, |n| n.max(0) as u64)
    };

    let action = match kind {
        Kind::Assistant => parts.and_then(model_action),
        Kind::User => parts.and_then(user_action),
        Kind::Other => None,
    };

    let tail = match kind {
        Kind::User => parts.map(user_tail_of).unwrap_or_default(),
        Kind::Assistant => parts.map(model_tail_of).unwrap_or_default(),
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
        tokens_in: tok("promptTokenCount"),
        tokens_out: tok("candidatesTokenCount"),
        action,
        cwd: None,    // Gemini CLI does not record cwd in transcripts
        branch: None, // Gemini CLI does not record git branch in transcripts
        permission_mode: None, // no equivalent in Gemini CLI
        tail,
    })
}

/// "Last relevant part wins": a `functionCall` reads as `tool: Name`,
/// otherwise a text preview. Mirrors `claude::assistant_action`.
fn model_action(parts: &[Value]) -> Option<String> {
    for part in parts.iter().rev() {
        if let Some(fc) = part.get("functionCall") {
            let name = fc.get("name").and_then(Value::as_str).unwrap_or("?");
            return Some(format!("tool: {name}"));
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            return Some(preview(text));
        }
    }
    None
}

fn user_action(parts: &[Value]) -> Option<String> {
    let has_response = parts
        .iter()
        .any(|p| p.get("functionResponse").is_some());
    if has_response {
        return Some("tool result".to_string());
    }
    // Plain user text
    for part in parts.iter() {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            return Some(format!("> {}", preview(text)));
        }
    }
    Some("> ...".to_string())
}

/// Classify the last meaningful part of a **model** turn for `Status`.
///
/// - `functionCall` → `Tail::ToolUse(id)`, falling back to the function name
///   when `id` is absent (Gemini API v1 omits it).
/// - `text` → `Tail::Text` (model turn ended cleanly; agent wants user input).
/// - anything else → `Tail::None`
fn model_tail_of(parts: &[Value]) -> Tail {
    for part in parts.iter().rev() {
        if let Some(fc) = part.get("functionCall") {
            let id = fc
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| fc.get("name").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            return Tail::ToolUse(id);
        }
        if part.get("text").is_some() {
            return Tail::Text;
        }
    }
    Tail::None
}

/// Classify the last meaningful part of a **user** turn for `Status`.
///
/// - `functionResponse` → `Tail::ToolResult(id)`, falling back to function
///   name when `id` is absent, so it can still resolve the matching ToolUse.
/// - plain `text` → `Tail::None` (a user prompt, not a block we resolve).
fn user_tail_of(parts: &[Value]) -> Tail {
    for part in parts.iter().rev() {
        if let Some(fr) = part.get("functionResponse") {
            let id = fr
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| fr.get("name").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            return Tail::ToolResult(id);
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

    // ── helpers ──────────────────────────────────────────────────────────────

    fn parse(line: &str) -> LineEvent {
        parse_line(line).expect("should parse")
    }

    // ── noise / non-JSON ─────────────────────────────────────────────────────

    #[test]
    fn non_json_returns_none() {
        assert!(parse_line("not json").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn unknown_role_is_other() {
        let ev = parse(r#"{"role":"system","parts":[{"text":"hi"}]}"#);
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tail, Tail::None);
    }

    // ── user turns ───────────────────────────────────────────────────────────

    #[test]
    fn plain_user_text() {
        let line = r#"{"role":"user","parts":[{"text":"hello gemini"}],"timestamp":"2024-01-15T10:00:00.000Z"}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.action.as_deref(), Some("> hello gemini"));
        assert_eq!(ev.tail, Tail::None);
        assert!(ev.ts_ms.is_some());
    }

    #[test]
    fn user_text_no_timestamp() {
        let ev = parse(r#"{"role":"user","parts":[{"text":"hi"}]}"#);
        assert_eq!(ev.kind, Kind::User);
        assert!(ev.ts_ms.is_none());
    }

    #[test]
    fn user_text_preview_truncated() {
        // 60-char string: preview should truncate at 46 chars + ellipsis
        let long = "a".repeat(60);
        let line = format!(r#"{{"role":"user","parts":[{{"text":"{long}"}}]}}"#);
        let ev = parse(&line);
        let action = ev.action.unwrap();
        assert!(action.ends_with('…'), "expected ellipsis, got: {action}");
        // "> " prefix (2) + up to 46 chars + "…" (1) = 49 max
        assert!(action.chars().count() <= 49, "action too long: {action}");
    }

    // ── model (assistant) turns ──────────────────────────────────────────────

    #[test]
    fn model_text_response() {
        let line = r#"{"role":"model","parts":[{"text":"Hello! How can I help?"}],"timestamp":"2024-01-15T10:00:01.000Z","model":"gemini-2.5-pro","usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":20}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(ev.tokens_in, 10);
        assert_eq!(ev.tokens_out, 20);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("Hello! How can I help?"));
    }

    #[test]
    fn model_function_call_with_id() {
        let line = r#"{"role":"model","parts":[{"functionCall":{"name":"bash","args":{"cmd":"ls"},"id":"fc_001"}}],"timestamp":"2024-01-15T10:00:02.000Z"}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.action.as_deref(), Some("tool: bash"));
        assert_eq!(ev.tail, Tail::ToolUse("fc_001".to_string()));
    }

    #[test]
    fn model_function_call_no_id_falls_back_to_name() {
        // Gemini API v1 may omit id; fall back to name for pairing
        let line = r#"{"role":"model","parts":[{"functionCall":{"name":"read_file","args":{"path":"x.rs"}}}]}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::ToolUse("read_file".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: read_file"));
    }

    #[test]
    fn model_last_part_wins_text_after_function_call() {
        // If a model turn has both functionCall and text, last part wins
        let line = r#"{"role":"model","parts":[{"functionCall":{"name":"bash","id":"fc_x","args":{}}},{"text":"Running bash…"}]}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("Running bash…"));
    }

    #[test]
    fn model_last_part_wins_function_call_after_text() {
        let line = r#"{"role":"model","parts":[{"text":"I'll run this:"},{"functionCall":{"name":"bash","id":"fc_y","args":{}}}]}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::ToolUse("fc_y".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: bash"));
    }

    // ── tool results (user functionResponse) ─────────────────────────────────

    #[test]
    fn user_function_response_with_id() {
        let line = r#"{"role":"user","parts":[{"functionResponse":{"name":"bash","id":"fc_001","response":{"output":"src/"}}}],"timestamp":"2024-01-15T10:00:03.000Z"}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.action.as_deref(), Some("tool result"));
        assert_eq!(ev.tail, Tail::ToolResult("fc_001".to_string()));
    }

    #[test]
    fn user_function_response_no_id_falls_back_to_name() {
        let line = r#"{"role":"user","parts":[{"functionResponse":{"name":"read_file","response":{"text":"hello"}}}]}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::ToolResult("read_file".to_string()));
    }

    // ── token accounting ─────────────────────────────────────────────────────

    #[test]
    fn tokens_default_zero_when_absent() {
        let ev = parse(r#"{"role":"model","parts":[{"text":"hi"}]}"#);
        assert_eq!(ev.tokens_in, 0);
        assert_eq!(ev.tokens_out, 0);
    }

    #[test]
    fn tokens_parsed_from_usage_metadata() {
        let line = r#"{"role":"model","parts":[{"text":"hi"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50}}"#;
        let ev = parse(line);
        assert_eq!(ev.tokens_in, 100);
        assert_eq!(ev.tokens_out, 50);
    }

    // ── fields always absent for Gemini CLI ──────────────────────────────────

    #[test]
    fn cwd_branch_permission_mode_always_none() {
        let line = r#"{"role":"user","parts":[{"text":"test"}],"cwd":"/some/path","gitBranch":"main","permissionMode":"default"}"#;
        let ev = parse(line);
        // Gemini CLI doesn't emit these; we ignore them even if present
        assert!(ev.cwd.is_none());
        assert!(ev.branch.is_none());
        assert!(ev.permission_mode.is_none());
    }

    // ── timestamp parsing ────────────────────────────────────────────────────

    #[test]
    fn timestamp_with_millis() {
        let line = r#"{"role":"user","parts":[{"text":"hi"}],"timestamp":"2024-06-01T12:00:00.500Z"}"#;
        let ev = parse(line);
        assert!(ev.ts_ms.is_some());
        // 2024-06-01T12:00:00.500Z → should end in 500 ms
        assert_eq!(ev.ts_ms.unwrap() % 1000, 500);
    }

    #[test]
    fn timestamp_without_millis() {
        let line = r#"{"role":"model","parts":[{"text":"hi"}],"timestamp":"2024-01-01T00:00:00Z"}"#;
        let ev = parse(line);
        assert!(ev.ts_ms.is_some());
    }

    #[test]
    fn malformed_timestamp_becomes_none() {
        let line = r#"{"role":"user","parts":[{"text":"hi"}],"timestamp":"not-a-date"}"#;
        let ev = parse(line);
        assert!(ev.ts_ms.is_none());
    }

    // ── empty / degenerate inputs ─────────────────────────────────────────────

    #[test]
    fn empty_parts_array() {
        let ev = parse(r#"{"role":"model","parts":[]}"#);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::None);
        assert!(ev.action.is_none());
    }

    #[test]
    fn missing_parts_field() {
        let ev = parse(r#"{"role":"user"}"#);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
    }
}
