//! Format adapter for Claude Code's on-disk session transcripts
//! (`~/.claude/projects/<project>/<session>.jsonl`).
//!
//! This is deliberately an *adapter*: the format is unstable vendor
//! territory, so everything agsess needs from a line is squeezed into
//! [`LineEvent`] here, anchored to recorded fixtures in the tests. Unknown
//! line types and malformed lines degrade to `Other`/`None`, never to a
//! crash — a monitor must survive whatever the transcript throws at it.
//!
//! Ported verbatim from agtop's `claude.rs`, then *extended* (additive only)
//! with the fields the status derivation needs: the per-line `permission_mode`
//! (from dedicated `permission-mode` records and from the top-level
//! `permissionMode` on `user` records), and the shape of the last content
//! block ([`Tail`]) — a `tool_use` with its `id`, a `tool_result` with its
//! `tool_use_id`, or a trailing `text`. agtop's aggregate never reads these,
//! so its rendering is unaffected; only [`crate::sessions`]'s `Status`
//! derivation consumes them.

use json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    User,
    Assistant,
    Other,
}

/// The shape of a line's final content block, as far as `Status` cares.
///
/// A line may end in a tool call (`ToolUse`, carrying the call id so a later
/// `tool_result` can resolve it), a tool result (`ToolResult`, carrying the
/// id it resolves), a trailing assistant `Text` block, or nothing relevant
/// (`None` — e.g. a bare user prompt string, or a non user/assistant record).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Tail {
    /// Last block is a `tool_use`; the `String` is its `id`.
    ToolUse(String),
    /// Last block is a `tool_result`; the `String` is its `tool_use_id`.
    ToolResult(String),
    /// Last block is a `text` block (an assistant turn ending cleanly).
    Text,
    #[default]
    None,
}

/// What one transcript line contributes to a session's statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct LineEvent {
    pub kind: Kind,
    pub ts_ms: Option<u64>,
    pub model: Option<String>,
    /// New input tokens billed for this turn (input + cache creation).
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Human-oriented "what just happened" string, if the line has one.
    pub action: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// The session's permission mode as recorded on this line, if any.
    /// Set by dedicated `permission-mode` records and by the top-level
    /// `permissionMode` on `user` records. `None` on lines that don't carry
    /// it — the aggregate keeps the last non-`None` value it saw.
    pub permission_mode: Option<String>,
    /// The shape of this line's final content block (for `Status`).
    pub tail: Tail,
}

/// Parse one transcript line. `None` means the line is not JSON — a monitor
/// treats that as noise, not an error.
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let v = json::parse(line).ok()?;
    let kind = match v.get("type").and_then(Value::as_str) {
        Some("user") => Kind::User,
        Some("assistant") => Kind::Assistant,
        _ => Kind::Other,
    };
    let msg = v.get("message");
    let usage = msg.and_then(|m| m.get("usage"));
    let tok = |key: &str| -> u64 {
        usage
            .and_then(|u| u.get(key))
            .and_then(Value::as_i64)
            .map_or(0, |n| n.max(0) as u64)
    };
    let action = match kind {
        Kind::Assistant => msg.and_then(assistant_action),
        Kind::User => msg.and_then(user_action),
        Kind::Other => None,
    };
    // permissionMode is recorded two ways: as a dedicated `permission-mode`
    // record (top-level `permissionMode`), and inline at the top level of
    // `user` records. Both live at the value's top level, so one read covers
    // them.
    let permission_mode = v
        .get("permissionMode")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let tail = match kind {
        Kind::Assistant | Kind::User => msg.map(tail_of).unwrap_or_default(),
        Kind::Other => Tail::None,
    };
    Some(LineEvent {
        kind,
        ts_ms: v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts),
        model: msg
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tokens_in: tok("input_tokens") + tok("cache_creation_input_tokens"),
        tokens_out: tok("output_tokens"),
        action,
        cwd: v.get("cwd").and_then(Value::as_str).map(str::to_string),
        branch: v
            .get("gitBranch")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        permission_mode,
        tail,
    })
}

/// The last content block wins: a tool call reads as `tool: Name`,
/// otherwise a preview of the text.
fn assistant_action(msg: &Value) -> Option<String> {
    let content = msg.get("content")?.as_array()?;
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

fn user_action(msg: &Value) -> Option<String> {
    match msg.get("content")? {
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

/// Classify a message's final *relevant* content block for `Status`.
///
/// Walks the content list from the end and returns the first block that
/// carries status meaning: a `tool_use` (with its `id`), a `tool_result`
/// (with its `tool_use_id`), or a `text` block. A `String` content (a plain
/// user prompt) is `Tail::None` — it is a prompt, not a block we resolve
/// against. This mirrors `assistant_action`'s "last block wins" walk, but
/// keeps identity (the ids) instead of a display string.
fn tail_of(msg: &Value) -> Tail {
    let Some(content) = msg.get("content") else {
        return Tail::None;
    };
    let blocks = match content.as_array() {
        Some(b) => b,
        None => return Tail::None, // a String prompt, etc.
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

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff]Z` (the transcript's timestamp shape)
/// into epoch milliseconds. Anything else returns `None`.
pub fn parse_ts(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> { s.get(range)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let millis = if b.len() > 19 && b[19] == b'.' {
        let frac: String = s[20..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(3)
            .collect();
        let mut ms = frac.parse::<i64>().ok().unwrap_or(0);
        for _ in frac.len()..3 {
            ms *= 10;
        }
        ms
    } else {
        0
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    if secs < 0 {
        return None;
    }
    Some(secs as u64 * 1000 + millis as u64)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
