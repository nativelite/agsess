# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-08-29

### Added
- `World` / `AgentSession`: discover Claude Code session transcripts under a
  projects root (`~/.claude/projects/*/<session>.jsonl`), incrementally tail
  only the bytes each file grew by (partial trailing line buffered until its
  newline; truncate/rewrite resets and re-aggregates), count subagent fan-out,
  and derive an attention `Status`. `World::new`, `refresh`, and
  `refresh_since(cutoff_ms)` (metadata-only for stale files, for a bounded cold
  start).
- `Status` derivation (`Working` / `WaitingApproval` / `WaitingPrompt` /
  `Idle`): a `permissionMode`-gated inference over the transcript tail. Pure
  `AgentSession::derive_status(now_ms)` (no system-clock read) with named,
  tunable thresholds `APPROVAL_DWELL_MS` (7 s) and `IDLE_MS` (60 s).
- `Vendor` enum (`ClaudeCode`) on every session, `first_seen_ms` stamp, and
  `default_root()` (`~/.claude/projects` via `USERPROFILE`/`HOME`).
- `claude` adapter: `LineEvent` / `parse_line` / `parse_ts`, ported from
  agtop and extended (additive) with `permission_mode` (from `permission-mode`
  records and inline `permissionMode` on `user` lines) and a `Tail`
  classification (`tool_use` id / `tool_result` id / trailing `text`) for the
  status derivation. Unknown and malformed lines degrade to `Other`/`None`,
  never a crash.
- Test suite: the adapter and incremental-tail tests carried over from agtop
  (adapted for `AgentSession`), plus status tests over synthetic fixtures, one
  per derivation row, driven with fixed `now_ms` values to exercise the
  dwell/idle thresholds deterministically. Fixtures model the real record
  shapes; no real transcript or conversation text is embedded.
- Stdlib-only `dev.py` runner (`check`, `test`, `fmt`, `guard`) and a Cargo.toml
  dependency guard (nativelite org crates only; the sole allowed dependency is
  `json`, with zero third-party/crates.io deps).

Depends only on the org crate `json` (the app-variant rule); third-party
dependencies remain forbidden. M1 of the amux 0.3 agent-aware feature: the
shared session reader extracted so agtop (later) and atrium can both consume it.

[Unreleased]: https://github.com/nativelite/agsess/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nativelite/agsess/releases/tag/v0.1.0
