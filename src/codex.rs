//! Format adapter for OpenAI Codex CLI's on-disk session rollout transcripts
//! (`~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`).
//!
//! Each line in a rollout file is a flat JSON object:
//!
//! ```text
//! {"timestamp":"2025-05-07T17:24:21.123Z","ordinal":1,"type":"<kind>","payload":{…}}
//! ```
//!
//! The `type` discriminants that drive status derivation:
//!
//! - `session_meta`: first line; `payload.cwd` (working dir) and `payload.git.branch`.
//! - `turn_context`: one per user turn; `payload.model`, `payload.cwd`, and
//!   `payload.approval_policy` (→ `permission_mode`).
//! - `response_item`: an OpenAI Responses API item; inner `payload.type`:
//!   - `function_call`        → `Kind::Assistant`, `Tail::ToolUse(call_id)`
//!   - `function_call_output` → `Kind::User`,      `Tail::ToolResult(call_id)`
//!   - `local_shell_call`     → `Kind::Assistant`, `Tail::ToolUse(call_id)`
//!   - `message` role=assistant → `Kind::Assistant`, `Tail::Text`
//!   - `message` role=user/developer → `Kind::User`, `Tail::None`
//!   - `reasoning`            → `Kind::Assistant`, `Tail::None` (thinking block)
//! - `event_msg`: legacy protocol events; inner `payload.type`:
//!   - `user_message`  → `Kind::User`, `Tail::None`
//!   - `agent_message` → `Kind::Assistant`, `Tail::Text`
//!   - `token_count`   → tokens only (`Kind::Other`)
//! - `token_usage_record`: per-turn token totals; tokens only (`Kind::Other`).
//!
//! ## Approval policy → permission_mode
//!
//! `turn_context.payload.approval_policy` maps to `permission_mode`:
//! - `"never"` → `"bypassPermissions"` (auto-approving; tool calls do not pause)
//! - anything else (including `"untrusted"`, `"on_request"`, granular objects) →
//!   `"default"` (prompting; an unresolved tool call past the dwell threshold
//!   escalates to `Status::WaitingApproval`)
//!
//! ## Session storage
//!
//! `$CODEX_HOME` (default `~/.codex`) → `sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
//! Archived sessions live under `archived_sessions/` in the same root.
//!
//! // NOTE: **unverified**: the line format was derived from the openai/codex
//! //   repository source (`codex-rs/protocol/src/models.rs`,
//! //   `codex-rs/history/src/rollout_payload.rs`,
//! //   `codex-rs/protocol/src/protocol.rs`) and from published third-party
//! //   analyses of real rollout files (PixelPaw-Labs/codex-trace, DEV.to).
//! //   It has NOT been tested against a live Codex CLI install.
//! //   Verify against a real `~/.codex/sessions/**/*.jsonl` before shipping.

use crate::claude::{parse_ts, Kind, LineEvent, Tail};
use json::Value;

/// Parse one Codex rollout JSONL line into a [`LineEvent`].
/// Returns `None` for non-JSON input or line types that contribute nothing
/// observable: the same contract as [`crate::claude::parse_line`].
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let v = json::parse(line).ok()?;
    let ts_ms = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
    let payload = v.get("payload")?;

    match v.get("type").and_then(Value::as_str) {
        Some("session_meta") => parse_session_meta(payload, ts_ms),
        Some("turn_context") => parse_turn_context(payload, ts_ms),
        Some("response_item") => parse_response_item(payload, ts_ms),
        Some("event_msg") => parse_event_msg(payload, ts_ms),
        Some("token_usage_record") => parse_token_usage_record(payload, ts_ms),
        _ => None,
    }
}

/// Session-start record. Carries the working directory and optional git branch.
fn parse_session_meta(p: &Value, ts_ms: Option<u64>) -> Option<LineEvent> {
    let cwd = p.get("cwd").and_then(Value::as_str).map(str::to_string);
    let branch = p
        .get("git")
        .and_then(|g| g.get("branch"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(LineEvent {
        kind: Kind::Other,
        ts_ms,
        model: None,
        tokens_in: 0,
        tokens_out: 0,
        action: None,
        cwd,
        branch,
        permission_mode: None,
        tail: Tail::None,
    })
}

/// Per-turn model/context record. Carries `model`, `cwd`, and `approval_policy`.
fn parse_turn_context(p: &Value, ts_ms: Option<u64>) -> Option<LineEvent> {
    let model = p
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let cwd = p.get("cwd").and_then(Value::as_str).map(str::to_string);
    // "never" is the only auto-approving policy; everything else (including
    // "untrusted", "on_request"/"on-failure", and granular objects) prompts.
    let ap = p.get("approval_policy");
    let permission_mode = match ap.and_then(Value::as_str) {
        Some("never") => Some("bypassPermissions".to_string()),
        Some(_) => Some("default".to_string()),
        None if ap.is_some() => Some("default".to_string()), // granular object
        None => None,
    };
    Some(LineEvent {
        kind: Kind::Other,
        ts_ms,
        model,
        tokens_in: 0,
        tokens_out: 0,
        action: None,
        cwd,
        branch: None,
        permission_mode,
        tail: Tail::None,
    })
}

/// OpenAI Responses API item. The inner `payload.type` distinguishes the item kind.
fn parse_response_item(p: &Value, ts_ms: Option<u64>) -> Option<LineEvent> {
    match p.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(LineEvent {
                kind: Kind::Assistant,
                ts_ms,
                model: None,
                tokens_in: 0,
                tokens_out: 0,
                action: Some(format!("tool: {name}")),
                cwd: None,
                branch: None,
                permission_mode: None,
                tail: Tail::ToolUse(call_id),
            })
        }

        Some("function_call_output")
        | Some("mcp_tool_call_output")
        | Some("custom_tool_call_output") => {
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(LineEvent {
                kind: Kind::User,
                ts_ms,
                model: None,
                tokens_in: 0,
                tokens_out: 0,
                action: Some("tool result".to_string()),
                cwd: None,
                branch: None,
                permission_mode: None,
                tail: Tail::ToolResult(call_id),
            })
        }

        Some("local_shell_call") => {
            // Shell commands use call_id (or id as a fallback) to link request → result.
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                .or_else(|| p.get("id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let cmd = p
                .get("action")
                .and_then(|a| a.get("command"))
                .and_then(Value::as_str);
            let action = cmd.map(|c| format!("shell: {}", preview(c)));
            Some(LineEvent {
                kind: Kind::Assistant,
                ts_ms,
                model: None,
                tokens_in: 0,
                tokens_out: 0,
                action,
                cwd: None,
                branch: None,
                permission_mode: None,
                tail: Tail::ToolUse(call_id),
            })
        }

        Some("message") => {
            let role = p.get("role").and_then(Value::as_str).unwrap_or("user");
            let (kind, tail) = match role {
                "assistant" => (Kind::Assistant, Tail::Text),
                _ => (Kind::User, Tail::None),
            };
            let action = if matches!(kind, Kind::Assistant) {
                p.get("content").and_then(|c| text_from_content(c))
            } else {
                None
            };
            Some(LineEvent {
                kind,
                ts_ms,
                model: None,
                tokens_in: 0,
                tokens_out: 0,
                action,
                cwd: None,
                branch: None,
                permission_mode: None,
                tail,
            })
        }

        Some("reasoning") => Some(LineEvent {
            kind: Kind::Assistant,
            ts_ms,
            model: None,
            tokens_in: 0,
            tokens_out: 0,
            action: None,
            cwd: None,
            branch: None,
            permission_mode: None,
            tail: Tail::None,
        }),

        _ => None,
    }
}

/// Legacy protocol event stream. The inner `payload.type` classifies the event.
fn parse_event_msg(p: &Value, ts_ms: Option<u64>) -> Option<LineEvent> {
    match p.get("type").and_then(Value::as_str) {
        Some("user_message") => {
            let msg = p.get("message").and_then(Value::as_str).unwrap_or("");
            let action = if msg.is_empty() {
                Some("> ...".to_string())
            } else {
                Some(format!("> {}", preview(msg)))
            };
            Some(LineEvent {
                kind: Kind::User,
                ts_ms,
                model: None,
                tokens_in: 0,
                tokens_out: 0,
                action,
                cwd: None,
                branch: None,
                permission_mode: None,
                tail: Tail::None,
            })
        }

        Some("agent_message") => {
            let msg = p.get("message").and_then(Value::as_str).unwrap_or("");
            Some(LineEvent {
                kind: Kind::Assistant,
                ts_ms,
                model: None,
                tokens_in: 0,
                tokens_out: 0,
                action: Some(preview(msg)),
                cwd: None,
                branch: None,
                permission_mode: None,
                tail: Tail::Text,
            })
        }

        // Token counts arrive via `event_msg/token_count` in addition to
        // `token_usage_record`. We prefer `last_token_usage` (current turn) over
        // the cumulative `total_token_usage`.
        Some("token_count") => {
            let info = p.get("info")?;
            let usage = info
                .get("last_token_usage")
                .or_else(|| info.get("total_token_usage"))?;
            let tokens_in = tok(usage, "input_tokens");
            let tokens_out = tok(usage, "output_tokens");
            Some(LineEvent {
                kind: Kind::Other,
                ts_ms,
                model: None,
                tokens_in,
                tokens_out,
                action: None,
                cwd: None,
                branch: None,
                permission_mode: None,
                tail: Tail::None,
            })
        }

        // task_started/turn_started: agent is beginning work.
        Some("task_started") | Some("turn_started") => Some(LineEvent {
            kind: Kind::Other,
            ts_ms,
            model: None,
            tokens_in: 0,
            tokens_out: 0,
            action: None,
            cwd: None,
            branch: None,
            permission_mode: None,
            tail: Tail::None,
        }),

        _ => None,
    }
}

/// Per-turn token totals. Prefer `turn_token_usage` (this turn) over `usage` (cumulative).
fn parse_token_usage_record(p: &Value, ts_ms: Option<u64>) -> Option<LineEvent> {
    let usage = p
        .get("turn_token_usage")
        .or_else(|| p.get("usage"))?;
    let tokens_in = tok(usage, "input_tokens");
    let tokens_out = tok(usage, "output_tokens");
    if tokens_in == 0 && tokens_out == 0 {
        return None;
    }
    Some(LineEvent {
        kind: Kind::Other,
        ts_ms,
        model: None,
        tokens_in,
        tokens_out,
        action: None,
        cwd: None,
        branch: None,
        permission_mode: None,
        tail: Tail::None,
    })
}

/// Extract a text preview from a Responses API `content` array.
/// Accepts both `output_text` (Responses API) and `text`/`input_text` (legacy) item types.
fn text_from_content(content: &Value) -> Option<String> {
    let arr = content.as_array()?;
    for item in arr.iter().rev() {
        match item.get("type").and_then(Value::as_str) {
            Some("output_text") | Some("text") | Some("input_text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    return Some(preview(text));
                }
            }
            _ => continue,
        }
    }
    None
}

fn tok(usage: &Value, key: &str) -> u64 {
    usage
        .get(key)
        .and_then(Value::as_i64)
        .map_or(0, |n| n.max(0) as u64)
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

    fn parse(line: &str) -> LineEvent {
        parse_line(line).expect("should parse")
    }

    // ── noise / non-JSON ──────────────────────────────────────────────────────

    #[test]
    fn non_json_returns_none() {
        assert!(parse_line("not json").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn unknown_type_returns_none() {
        let line = r#"{"timestamp":"2025-05-07T17:24:21.000Z","type":"compacted","payload":{"message":"context compacted"}}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn line_without_payload_returns_none() {
        let line = r#"{"timestamp":"2025-05-07T17:24:21.000Z","type":"session_meta"}"#;
        assert!(parse_line(line).is_none());
    }

    // ── session_meta ──────────────────────────────────────────────────────────

    #[test]
    fn session_meta_extracts_cwd() {
        let line = r#"{"timestamp":"2025-05-07T17:24:21.000Z","ordinal":1,"type":"session_meta","payload":{"session_id":"abc","id":"abc","timestamp":"2025-05-07T17:24:21Z","cwd":"/home/user/myproject","originator":"cli","cli_version":"0.130.0","model_provider":"openai"}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.cwd.as_deref(), Some("/home/user/myproject"));
        assert!(ev.branch.is_none());
        assert!(ev.ts_ms.is_some());
    }

    #[test]
    fn session_meta_extracts_git_branch() {
        let line = r#"{"timestamp":"2025-05-07T17:24:21.000Z","ordinal":1,"type":"session_meta","payload":{"session_id":"abc","id":"abc","timestamp":"2025-05-07T17:24:21Z","cwd":"/home/user/myproject","originator":"cli","cli_version":"0.130.0","git":{"branch":"main","root":"/home/user/myproject"}}}"#;
        let ev = parse(line);
        assert_eq!(ev.branch.as_deref(), Some("main"));
    }

    #[test]
    fn session_meta_empty_branch_becomes_none() {
        let line = r#"{"timestamp":"2025-05-07T17:24:21.000Z","ordinal":1,"type":"session_meta","payload":{"session_id":"abc","id":"abc","timestamp":"2025-05-07T17:24:21Z","cwd":"/home/user","originator":"cli","cli_version":"0.130.0","git":{"branch":""}}}"#;
        let ev = parse(line);
        assert!(ev.branch.is_none());
    }

    // ── turn_context ──────────────────────────────────────────────────────────

    #[test]
    fn turn_context_extracts_model_and_cwd() {
        let line = r#"{"timestamp":"2025-05-07T17:24:22.000Z","ordinal":2,"type":"turn_context","payload":{"model":"gpt-4o","cwd":"/home/user/myproject","approval_policy":"on_request","sandbox_policy":"workspace-write"}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.model.as_deref(), Some("gpt-4o"));
        assert_eq!(ev.cwd.as_deref(), Some("/home/user/myproject"));
        assert_eq!(ev.permission_mode.as_deref(), Some("default"));
    }

    #[test]
    fn turn_context_never_approval_is_bypass() {
        let line = r#"{"timestamp":"2025-05-07T17:24:22.000Z","ordinal":2,"type":"turn_context","payload":{"model":"gpt-4o","cwd":"/home/user","approval_policy":"never","sandbox_policy":"workspace-write"}}"#;
        let ev = parse(line);
        assert_eq!(ev.permission_mode.as_deref(), Some("bypassPermissions"));
    }

    #[test]
    fn turn_context_untrusted_approval_is_default() {
        let line = r#"{"timestamp":"2025-05-07T17:24:22.000Z","ordinal":2,"type":"turn_context","payload":{"model":"gpt-4o","cwd":"/home/user","approval_policy":"untrusted","sandbox_policy":"workspace-write"}}"#;
        let ev = parse(line);
        assert_eq!(ev.permission_mode.as_deref(), Some("default"));
    }

    #[test]
    fn turn_context_granular_object_is_default() {
        // A granular approval_policy is an object: treat as prompting.
        let line = r#"{"timestamp":"2025-05-07T17:24:22.000Z","ordinal":2,"type":"turn_context","payload":{"model":"gpt-4o","cwd":"/home/user","approval_policy":{"type":"granular","allow_apply_patch":true},"sandbox_policy":"workspace-write"}}"#;
        let ev = parse(line);
        assert_eq!(ev.permission_mode.as_deref(), Some("default"));
    }

    #[test]
    fn turn_context_missing_approval_policy_gives_none_mode() {
        let line = r#"{"timestamp":"2025-05-07T17:24:22.000Z","ordinal":2,"type":"turn_context","payload":{"model":"gpt-4o","cwd":"/home/user","sandbox_policy":"workspace-write"}}"#;
        let ev = parse(line);
        assert!(ev.permission_mode.is_none());
    }

    // ── response_item / function_call ─────────────────────────────────────────

    #[test]
    fn response_item_function_call() {
        let line = r#"{"timestamp":"2025-05-07T17:24:23.000Z","ordinal":3,"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"call_abc123","arguments":"{\"cmd\":\"ls\"}"}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::ToolUse("call_abc123".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool: shell"));
    }

    #[test]
    fn response_item_function_call_unknown_name() {
        // name field absent → falls back to "?"
        let line = r#"{"timestamp":"2025-05-07T17:24:23.000Z","ordinal":3,"type":"response_item","payload":{"type":"function_call","call_id":"call_xyz","arguments":"{}"}}"#;
        let ev = parse(line);
        assert_eq!(ev.action.as_deref(), Some("tool: ?"));
        assert_eq!(ev.tail, Tail::ToolUse("call_xyz".to_string()));
    }

    #[test]
    fn response_item_function_call_empty_call_id() {
        let line = r#"{"timestamp":"2025-05-07T17:24:23.000Z","ordinal":3,"type":"response_item","payload":{"type":"function_call","name":"read","call_id":"","arguments":"{}"}}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::ToolUse("".to_string()));
    }

    // ── response_item / function_call_output ──────────────────────────────────

    #[test]
    fn response_item_function_call_output() {
        let line = r#"{"timestamp":"2025-05-07T17:24:25.000Z","ordinal":5,"type":"response_item","payload":{"type":"function_call_output","call_id":"call_abc123","output":"src/ lib/ Cargo.toml"}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::ToolResult("call_abc123".to_string()));
        assert_eq!(ev.action.as_deref(), Some("tool result"));
    }

    #[test]
    fn response_item_mcp_tool_call_output() {
        let line = r#"{"timestamp":"2025-05-07T17:24:25.000Z","ordinal":5,"type":"response_item","payload":{"type":"mcp_tool_call_output","call_id":"mcp_001","output":{}}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::ToolResult("mcp_001".to_string()));
    }

    #[test]
    fn response_item_custom_tool_call_output() {
        let line = r#"{"timestamp":"2025-05-07T17:24:25.000Z","ordinal":5,"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"custom_001","output":"done"}}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::ToolResult("custom_001".to_string()));
    }

    // ── response_item / local_shell_call ──────────────────────────────────────

    #[test]
    fn response_item_local_shell_call_with_command() {
        let line = r#"{"timestamp":"2025-05-07T17:24:24.000Z","ordinal":4,"type":"response_item","payload":{"type":"local_shell_call","call_id":"lsc_001","status":"in_progress","action":{"type":"run","command":"cargo build"}}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::ToolUse("lsc_001".to_string()));
        assert_eq!(ev.action.as_deref(), Some("shell: cargo build"));
    }

    #[test]
    fn response_item_local_shell_call_falls_back_to_id() {
        // call_id absent, id present: use id as the tool use id
        let line = r#"{"timestamp":"2025-05-07T17:24:24.000Z","ordinal":4,"type":"response_item","payload":{"type":"local_shell_call","id":"lsc_legacy","status":"completed","action":{"type":"run","command":"ls"}}}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::ToolUse("lsc_legacy".to_string()));
    }

    #[test]
    fn response_item_local_shell_call_no_command() {
        let line = r#"{"timestamp":"2025-05-07T17:24:24.000Z","ordinal":4,"type":"response_item","payload":{"type":"local_shell_call","call_id":"lsc_002","status":"in_progress","action":{"type":"type","input":"hello"}}}"#;
        let ev = parse(line);
        assert!(ev.action.is_none());
        assert_eq!(ev.tail, Tail::ToolUse("lsc_002".to_string()));
    }

    // ── response_item / message ───────────────────────────────────────────────

    #[test]
    fn response_item_message_assistant() {
        let line = r#"{"timestamp":"2025-05-07T17:24:26.000Z","ordinal":6,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Here is the result."}]}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("Here is the result."));
    }

    #[test]
    fn response_item_message_assistant_text_type() {
        // Older format uses "text" instead of "output_text"
        let line = r#"{"timestamp":"2025-05-07T17:24:26.000Z","ordinal":6,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"Done!"}]}}"#;
        let ev = parse(line);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(ev.action.as_deref(), Some("Done!"));
    }

    #[test]
    fn response_item_message_assistant_preview_truncated() {
        let long = "x".repeat(60);
        let line = format!(
            r#"{{"timestamp":"2025-05-07T17:24:26.000Z","ordinal":6,"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{long}"}}]}}}}"#
        );
        let ev = parse(&line);
        let action = ev.action.unwrap();
        assert!(action.ends_with('…'), "expected ellipsis: {action}");
        assert!(action.chars().count() <= 47, "too long: {action}");
    }

    #[test]
    fn response_item_message_user() {
        let line = r#"{"timestamp":"2025-05-07T17:24:22.500Z","ordinal":3,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"write a todo app"}]}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
        assert!(ev.action.is_none());
    }

    #[test]
    fn response_item_message_developer_role_is_user_kind() {
        let line = r#"{"timestamp":"2025-05-07T17:24:22.500Z","ordinal":3,"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system prompt"}]}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
    }

    #[test]
    fn response_item_message_empty_content_array() {
        let line = r#"{"timestamp":"2025-05-07T17:24:26.000Z","ordinal":6,"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        assert!(ev.action.is_none());
    }

    // ── response_item / reasoning ─────────────────────────────────────────────

    #[test]
    fn response_item_reasoning_is_assistant_other_tail() {
        let line = r#"{"timestamp":"2025-05-07T17:24:23.500Z","ordinal":4,"type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"encr..."}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::None);
    }

    // ── event_msg / user_message ──────────────────────────────────────────────

    #[test]
    fn event_msg_user_message() {
        let line = r#"{"timestamp":"2025-05-07T17:24:22.000Z","ordinal":3,"type":"event_msg","payload":{"type":"user_message","message":"write a hello world app"}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.tail, Tail::None);
        assert_eq!(ev.action.as_deref(), Some("> write a hello world app"));
    }

    #[test]
    fn event_msg_user_message_empty_string() {
        let line = r#"{"timestamp":"2025-05-07T17:24:22.000Z","ordinal":3,"type":"event_msg","payload":{"type":"user_message","message":""}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::User);
        assert_eq!(ev.action.as_deref(), Some("> ..."));
    }

    // ── event_msg / agent_message ─────────────────────────────────────────────

    #[test]
    fn event_msg_agent_message() {
        let line = r#"{"timestamp":"2025-05-07T17:24:27.000Z","ordinal":8,"type":"event_msg","payload":{"type":"agent_message","message":"I've created the hello world app."}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Assistant);
        assert_eq!(ev.tail, Tail::Text);
        assert_eq!(
            ev.action.as_deref(),
            Some("I've created the hello world app.")
        );
    }

    // ── event_msg / token_count ───────────────────────────────────────────────

    #[test]
    fn event_msg_token_count_last_usage() {
        let line = r#"{"timestamp":"2025-05-07T17:24:27.500Z","ordinal":9,"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":120,"output_tokens":45},"total_token_usage":{"input_tokens":500,"output_tokens":200}}}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tokens_in, 120);
        assert_eq!(ev.tokens_out, 45);
    }

    #[test]
    fn event_msg_token_count_falls_back_to_total() {
        let line = r#"{"timestamp":"2025-05-07T17:24:27.500Z","ordinal":9,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"output_tokens":200}}}}"#;
        let ev = parse(line);
        assert_eq!(ev.tokens_in, 500);
        assert_eq!(ev.tokens_out, 200);
    }

    #[test]
    fn event_msg_token_count_no_info_returns_none() {
        let line = r#"{"timestamp":"2025-05-07T17:24:27.500Z","ordinal":9,"type":"event_msg","payload":{"type":"token_count"}}"#;
        assert!(parse_line(line).is_none());
    }

    // ── event_msg / task_started ──────────────────────────────────────────────

    #[test]
    fn event_msg_task_started_is_other() {
        let line = r#"{"timestamp":"2025-05-07T17:24:23.000Z","ordinal":4,"type":"event_msg","payload":{"type":"task_started","turn_id":"turn_abc"}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tail, Tail::None);
    }

    #[test]
    fn event_msg_turn_started_alias() {
        let line = r#"{"timestamp":"2025-05-07T17:24:23.000Z","ordinal":4,"type":"event_msg","payload":{"type":"turn_started","turn_id":"turn_abc"}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Other);
    }

    #[test]
    fn event_msg_unknown_subtype_returns_none() {
        let line = r#"{"timestamp":"2025-05-07T17:24:27.000Z","ordinal":8,"type":"event_msg","payload":{"type":"context_compacted"}}"#;
        assert!(parse_line(line).is_none());
    }

    // ── token_usage_record ────────────────────────────────────────────────────

    #[test]
    fn token_usage_record_prefers_turn_usage() {
        let line = r#"{"timestamp":"2025-05-07T17:24:28.000Z","ordinal":10,"type":"token_usage_record","payload":{"turn_id":"turn_abc","usage":{"input_tokens":500,"output_tokens":200},"turn_token_usage":{"input_tokens":80,"output_tokens":30}}}"#;
        let ev = parse(line);
        assert_eq!(ev.kind, Kind::Other);
        assert_eq!(ev.tokens_in, 80);
        assert_eq!(ev.tokens_out, 30);
    }

    #[test]
    fn token_usage_record_falls_back_to_usage() {
        let line = r#"{"timestamp":"2025-05-07T17:24:28.000Z","ordinal":10,"type":"token_usage_record","payload":{"turn_id":"turn_abc","usage":{"input_tokens":500,"output_tokens":200}}}"#;
        let ev = parse(line);
        assert_eq!(ev.tokens_in, 500);
        assert_eq!(ev.tokens_out, 200);
    }

    #[test]
    fn token_usage_record_all_zero_returns_none() {
        let line = r#"{"timestamp":"2025-05-07T17:24:28.000Z","ordinal":10,"type":"token_usage_record","payload":{"turn_id":"turn_abc","turn_token_usage":{"input_tokens":0,"output_tokens":0}}}"#;
        assert!(parse_line(line).is_none());
    }

    // ── timestamp parsing ─────────────────────────────────────────────────────

    #[test]
    fn timestamp_with_millis() {
        let line = r#"{"timestamp":"2025-05-07T17:24:21.500Z","ordinal":1,"type":"session_meta","payload":{"session_id":"abc","id":"abc","timestamp":"","cwd":"/home/user","originator":"cli","cli_version":"0.1"}}"#;
        let ev = parse(line);
        assert!(ev.ts_ms.is_some());
        assert_eq!(ev.ts_ms.unwrap() % 1000, 500);
    }

    #[test]
    fn malformed_timestamp_becomes_none() {
        let line = r#"{"timestamp":"not-a-date","ordinal":1,"type":"session_meta","payload":{"session_id":"abc","id":"abc","timestamp":"","cwd":"/home/user","originator":"cli","cli_version":"0.1"}}"#;
        let ev = parse(line);
        assert!(ev.ts_ms.is_none());
    }

    // ── status derivation scenario ────────────────────────────────────────────
    // End-to-end: simulate a turn with function_call → function_call_output →
    // agent text reply, and verify the tail sequence.

    #[test]
    fn status_scenario_tool_call_then_result_then_text() {
        let call_line = r#"{"timestamp":"2025-05-07T17:24:24.000Z","ordinal":4,"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"tool_1","arguments":"{}"}}"#;
        let result_line = r#"{"timestamp":"2025-05-07T17:24:25.000Z","ordinal":5,"type":"response_item","payload":{"type":"function_call_output","call_id":"tool_1","output":"ok"}}"#;
        let text_line = r#"{"timestamp":"2025-05-07T17:24:26.000Z","ordinal":6,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"All done."}]}}"#;

        let ev_call = parse(call_line);
        assert_eq!(ev_call.kind, Kind::Assistant);
        assert_eq!(ev_call.tail, Tail::ToolUse("tool_1".to_string()));

        let ev_result = parse(result_line);
        assert_eq!(ev_result.kind, Kind::User);
        assert_eq!(ev_result.tail, Tail::ToolResult("tool_1".to_string()));

        let ev_text = parse(text_line);
        assert_eq!(ev_text.kind, Kind::Assistant);
        assert_eq!(ev_text.tail, Tail::Text);
        assert_eq!(ev_text.action.as_deref(), Some("All done."));
    }
}
