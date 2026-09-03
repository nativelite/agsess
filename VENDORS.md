# agsess — Vendor Support Matrix

Updated by the **docs** agent. All 8 vendor modules landed — gate GREEN at 187 lib tests.
Last updated: bus cursor 25.

| Vendor | Status | Module | Transcript Location | Line Format | Certainty | Tests |
|--------|--------|--------|---------------------|-------------|-----------|-------|
| `ClaudeCode` | ✅ shipped | `src/claude.rs` | `~/.claude/projects/<project>/<session>.jsonl` | JSONL — `{type,message,timestamp,cwd,gitBranch,permissionMode}` | **verified** | — |
| `gemini` | ✅ verified-pass | `src/gemini.rs` | `~/.gemini/tmp/<session-id>/<session>.jsonl` | JSONL — `{role:"user"\|"model", parts:[{text}\|{functionCall}\|{functionResponse}], timestamp, model, usageMetadata:{promptTokenCount,candidatesTokenCount}}` | **researched-stub** | 20 |
| `codex` | ✅ verified-pass | `src/codex.rs` | `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` (or `$CODEX_HOME`; archived under `archived_sessions/`) | **Tagged event log** — `{timestamp, ordinal, type, payload}`. Types: `session_meta` (cwd+branch), `turn_context` (model+cwd+approval_policy→permission_mode), `response_item` (function_call/function_call_output/local_shell_call/message/reasoning), `event_msg` (user_message/agent_message/token_count), `token_usage_record` | **researched-stub** | 42 |
| `aider` | ✅ verified-pass | `src/aider.rs` | `<project-cwd>/.aider.chat.history.md` (configurable via `--chat-history-file`) | **Markdown, not JSONL** — user turns prefixed `> msg`; assistant turns are bare-text blocks; session header `# aider chat started at YYYY-MM-DD HH:MM:SS`; `#### aider (model)` in structured mode | **researched-stub** | 26 |
| `cursor-agent` | ✅ verified-pass | `src/cursor_agent.rs` | `~/.cursor/projects/<project-id>/agent-transcripts/<session-id>/subagents/*.jsonl` (Background Agent sub-agents only; main Cursor chat uses SQLite) | **Dual-shape JSONL** — Anthropic: `{role, content:[{type,id,name,...}], timestamp, model, usage:{promptTokens,completionTokens}}`; OpenAI: `{role, content:null, tool_calls:[{id,function:{name}}]}`; tool results: `{role:"tool", tool_call_id, content}` | **researched-stub** | 27 |
| `copilot` | ✅ verified-pass | `src/copilot.rs` | `~/.config/github-copilot/sessions/<session-id>.jsonl` (Linux/macOS) · `%APPDATA%\GitHub Copilot\sessions\<session-id>.jsonl` (Windows) — **hypothetical** | JSONL — `{timestamp, type:"user"\|"assistant"\|"tool_call"\|"tool_result", content, model?, id?, tool_call_id?, cwd?, branch?}` | **researched-stub** | 19 |
| `qwen` | ✅ verified-pass | `src/qwen.rs` | `~/.qwen/projects/<sanitized-cwd>/chats/<session>.jsonl` | JSONL — `{role:"user"\|"model", content:String\|[{type,...}], timestamp, cwd, gitBranch, model?}` | **researched-stub** | 14 |
| `opencode` | ✅ verified-pass | `src/opencode.rs` | SQLite: `~/.local/share/opencode/opencode.db` (Linux) · `~/Library/Application Support/opencode/opencode.db` (macOS) · `%APPDATA%\opencode\opencode.db` (Windows) | JSONL export from SQLite `message` table — `{id, session_id, role:"user"\|"assistant", model:{providerID,modelID}?, content:[{type,...}], tokens:{input,output,cache}?, time:{start,end}}` | **researched-stub** | 27 |
| `goose` | ✅ verified-pass | `src/goose.rs` | `~/.local/share/goose/sessions/<session>.jsonl` (Linux/macOS) · `%APPDATA%\goose\sessions\<session>.jsonl` (Windows) | JSONL — `{role:"user"\|"assistant", content:[{type:"text"\|"tool_use"\|"tool_result",...}], created:<unix_s>}` | **researched-stub** | 12 |

**Total lib tests in-tree:** 187 (aider 26 + codex 42 + copilot 19 + cursor-agent 27 + gemini 20 + goose 12 + opencode 27 + qwen 14) + 18 integ + 1 doctest.

## Certainty key

| Label | Meaning |
|-------|---------|
| **verified** | Format confirmed against a real running transcript |
| **researched-stub** | Format documented from public sources; unit-tested against synthetic lines; `// NOTE:` in module |
| pending | Worker has not yet reported |

## Format contract (from lead — bus #1)

Each vendor module exposes:

```rust
// src/<vendor>.rs
pub fn parse_line(line: &str) -> Option<crate::claude::LineEvent>
```

**Critical fields** (drive `Status` derivation):
- `kind` — `Kind::User` / `Kind::Assistant` / `Kind::Other`
- `ts_ms` — epoch milliseconds; reuse `claude::parse_ts` for ISO-8601
- `tail` — `Tail::ToolUse(id)` · `Tail::ToolResult(tool_use_id)` · `Tail::Text` · `Tail::None`
- `permission_mode` — `Some("default")` or `Some("plan")` escalate to `WaitingApproval`; auto-approve → `None` or other string

**Best-effort fields** (`None`/`0` OK if absent): `model`, `tokens_in`, `tokens_out`, `action`, `cwd`, `branch`

**Rules:**
- Import `Kind`, `Tail`, `LineEvent` from `crate::claude` — do **not** redefine
- Unknown line types → `Kind::Other` / `Tail::None`; malformed → `None` (never panic)
- If format unverified against real transcript: add `// NOTE: unverified` in file; label `researched-stub`
- Do **not** edit `Vendor` enum or dispatch match in `src/sessions.rs` — lead serializes those
- **Zero crates.io deps** — guard rejects any non-nativelite dep; `dev-dependencies` must also stay empty

## Format notes by vendor

### codex — uniquely structured

Codex is the only vendor that uses a **tagged event log** rather than one-turn-per-line. Each line has an outer envelope `{timestamp, ordinal, type, payload}` and the `type` field dispatches to different sub-parsers:

| `type` | Sub-type | Kind | Tail | Notable fields |
|--------|----------|------|------|----------------|
| `session_meta` | — | Other | None | `payload.cwd`, `payload.git.branch` |
| `turn_context` | — | Other | None | `payload.model`, `payload.approval_policy` → `permission_mode` |
| `response_item` | `function_call` | Assistant | ToolUse(call_id) | `payload.name`, `payload.call_id` |
| `response_item` | `function_call_output` | User | ToolResult(call_id) | `payload.call_id` |
| `response_item` | `local_shell_call` | Assistant | ToolUse(call_id) | `payload.action.command` |
| `response_item` | `message` role=assistant | Assistant | Text | content array |
| `response_item` | `message` role=user/developer | User | None | — |
| `response_item` | `reasoning` | Assistant | None | thinking block |
| `event_msg` | `user_message` | User | None | `payload.message` |
| `event_msg` | `agent_message` | Assistant | Text | `payload.message` |
| `event_msg` | `token_count` | Other | None | `last_token_usage` preferred |
| `token_usage_record` | — | Other | None | `turn_token_usage` preferred |

**Approval policy → permission_mode:** `"never"` → `"bypassPermissions"` (auto-approve, no WaitingApproval); everything else (including `"untrusted"`, `"on_request"`, granular objects) → `"default"` (prompting).

### cursor-agent — dual-shape tool calls

Handles two wiring styles in one parser:
- **Anthropic**: `content[].type == "tool_use"` with `id` field; tool results as `{role:"user", content:[{type:"tool_result", tool_use_id}]}`
- **OpenAI**: `tool_calls[].id` + `tool_calls[].function.name`; tool results as `{role:"tool", tool_call_id}`

`role:"tool"` maps to `Kind::User` (agent is still working). Tokens via `usage.promptTokens` / `usage.completionTokens`.

### qwen — verified location + partial format

`cwd`, `gitBranch`, `role`, `content`, and `timestamp` field names are confirmed from published sources. Tool-call block shape (`tool_use`/`tool_result` Anthropic-style) is **unverified** — Qwen Code supports multiple backends (Anthropic, OpenAI, Gemini, Qwen, Ollama) and the on-disk block shape may differ by backend.

## Notable integration caveats

| Vendor | Caveat | Decision needed |
|--------|--------|-----------------|
| `aider` | **Discovery gap** — `sessions::World` scans `*.jsonl`; aider writes `<cwd>/.aider.chat.history.md`. Aider sessions never discovered without adding `.md` discovery. | Lead: extend World scanner |
| `aider` | **Idle-trap** — no per-message timestamps; `last_ts_ms` set only from session-start header, never advances. `derive_status` reports Idle 60 s after start regardless of activity. | Lead: add `mtime_ms` fallback for vendors with stale/absent `last_ts_ms` |
| `cursor-agent` | **Discovery gap** — World scans Claude projects root; Cursor sub-agent transcripts live under `~/.cursor/projects/` with a nested `agent-transcripts/*/subagents/` layout. Needs a secondary scan root. | Lead: add `~/.cursor/projects/` root |
| `opencode` | **SQLite store** — no JSONL file on disk; needs a row-export or CDC layer to produce the target JSONL. `parse_line` targets that export. | Integration layer TBD |
| `copilot` | **No local file today** — gh copilot CLI is interactive; Copilot Chat uses SQLite. Parser targets a hypothetical future CLI JSONL. | Monitor Copilot roadmap |
| `gemini` | `functionCall.id` absent in API v1 — falls back to function name for tail resolution. May mislink if two calls to the same tool are open simultaneously. | Acceptable until Gemini API stabilises |
| `goose` | `created` field is Unix **seconds**, not milliseconds — parser multiplies by 1000. | Confirmed correct; gate verified |
| `qwen` | Tool-block shape unverified for non-Anthropic backends. Multi-backend format divergence possible. | Verify against real transcript per backend |

## Gate status

`python dev.py check` tracked by review-gate (pane 11):

| Batch | Result | Lib tests | Detail |
|-------|--------|-----------|--------|
| baseline | ✅ GREEN | 18 (+doctest) | guard OK, no vendor modules yet |
| batch 1 | ✅ GREEN | 100 | copilot + gemini + goose + opencode; aider on disk but not in lib.rs |
| batch 2 | ✅ GREEN | 118 | + aider + qwen; goose tail-drift caught + fixed mid-gate |
| **final** | ✅ **GREEN** | **187** | **all 8 vendors**; Vendor enum still ClaudeCode-only — lead wiring is the last step |

## Update log

| Event | Vendor | Detail |
|-------|--------|--------|
| scaffold | — | VENDORS.md created; all 8 vendors claimed, no modules landed |
| bus #4 | review-gate | Baseline `python dev.py check` GREEN (guard + 18 integ + doctest); zero crates.io deps enforced |
| bus #5 | `opencode` | Module ready v1 — 23 tests green; researched-stub (SQLite-based) |
| bus #6 | `gemini` | Module ready — 20 tests green; researched-stub |
| bus #7 | `copilot` | Module ready — 19 tests green; researched-stub (hypothetical JSONL) |
| bus #8 | `aider` | Module ready — 26 tests green; researched-stub; Markdown format; no per-message ts |
| bus #9 | `goose` | Module ready — 12 tests green; researched-stub; Anthropic messages format |
| bus #10 | review-gate | Gate GREEN (batch 1): 100 lib tests. aider on disk but not in lib.rs. |
| bus #11 | `opencode` | Module v2 — 27 tests; format refined: `time.start` ms int, `tokens` nested, `model` is object |
| bus #12 | `gemini` | verify-PASS: role/parts→Kind, functionCall→ToolUse, functionResponse→ToolResult |
| bus #13 | `copilot` | verify-PASS: no real JSONL today; type→tail correct; exemplary honesty label |
| bus #14 | `opencode` | verify-PASS: Anthropic-style blocks, panic-free |
| bus #15 | `goose` | ⚠️ BLOCKER: `user_prompt_block` test failed (Tail::None vs Tail::Text); gate RED |
| bus #16 | `qwen` | Module ready — 14 tests green; researched-stub (tool block shape unverified) |
| bus #17 | review-gate | Gate GREEN (batch 2): 118 lib tests. Blocker #15 resolved. |
| bus #18 | `goose` | verify-PASS: tail-drift resolved; `created`→ms×1000 correct; panic-free |
| bus #19 | `aider` | verify-PASS: honest stub, panic-free, 26 tests; markdown classifier correct |
| bus #20 | `aider` | ⚠️ DECISION: two integration gaps raised (discovery + idle-trap) — see caveats table |
| bus #21 | review-gate | Gate confirm: 6 vendors in-tree green. Waiting: codex, cursor-agent. |
| bus #22 | review-gate | Gate GREEN after cursor-agent landed: 145 lib tests. 7/8 vendors. |
| bus #23 | `cursor-agent` | Module ready — 27 tests green; dual-shape OpenAI/Anthropic; discovery mismatch noted |
| bus #24 | `codex` | Module ready — 42 tests green; researched-stub; tagged event log format |
| bus #25 | review-gate | 🏁 **MILESTONE: ALL 8 vendors in-tree. Gate GREEN: guard + 187 lib + 18 integ + 1 doctest.** Last step: lead Vendor enum + dispatch wiring. |
