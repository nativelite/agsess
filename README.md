# agsess

**Local agent session state** — the JSONL transcripts a coding-agent CLI writes
to the user's own disk in, a snapshot of `AgentSession`s (each with a derived
attention `Status`) out.

Part of the nativelite **agent terminal** suite. It is the reader that
[agtop](https://github.com/nativelite/agtop) and
[amux](https://github.com/nativelite/amux) share: agtop renders it as a live
table; amux binds each pane to the session its hosted agent is writing and
tints the pane border when an agent is waiting on the human.

- **Zero third-party dependencies.** Standard library plus the org crate
  [`json`](https://github.com/nativelite/json-rs) only. Nothing from crates.io.
- **One concern.** Files → session state. It does not render, does not own a
  loop, does not spawn or control anything, and **never writes**.
- **Read-only, own disk, documented surfaces only.** It opens `*.jsonl` under
  the configured projects root (`~/.claude/projects`) and its
  `<session>/subagents/` dirs — the user's own tools' files, nothing else in
  `~/.claude`, never a credential file, no network. It **never writes**. A
  consumer that only needs attention (like amux) reads `status` and ignores the
  message previews (`last_action`/`recent`) — status, not conversation content.

## Use

```rust
let mut world = agsess::World::new(agsess::default_root());
world.refresh();
for s in &world.sessions {
    println!("{:?} {} {:?}", s.vendor, s.project_name(), s.status);
}
```

`World::refresh()` discovers new transcripts, reads only the bytes each grew by
since last time (a partial trailing line is buffered until its newline arrives;
a truncated/rewritten file resets and re-aggregates), refreshes subagent counts,
and derives each session's `Status`.

`World::refresh_since(cutoff_ms)` is the same, but skips *tailing* any transcript
whose mtime predates the cutoff — metadata only. A multiplexer passes its own
process start time so a cold first scan of a large history never blocks input on
sessions that stopped writing before it existed.

## The status derivation

`Status` is the public contract; how it is derived is a private, unstable
adapter detail (and may later be fed by a hook instead of inferred). Today it is
an inference over the transcript tail, **gated by `permissionMode`** so a busy
tool is never mistaken for a prompt:

| Tail shape | Status |
| --- | --- |
| trailing `tool_use`, unresolved, mode prompts (`default`/`plan`), dwell exceeded | `WaitingApproval` (`Working` before the dwell) |
| trailing `tool_use`, unresolved, mode auto-approves (`auto`/`acceptEdits`/…) | `Working` |
| last line is an assistant turn ending in `text` | `WaitingPrompt` |
| last line is a `user` prompt or tool result | `Working` |
| nothing within `IDLE_MS` of now | `Idle` |

`derive_status(now_ms)` is **pure**: it never reads the system clock, so the
dwell and idle thresholds are deterministically testable. `APPROVAL_DWELL_MS`
(default 7 s, a 6–8 s band) and `IDLE_MS` (default 60 s) are named, documented,
tunable constants.

On a machine running mostly in `auto` mode the loud `WaitingApproval` state
correctly almost never fires — auto mode does not block on the human, so there
is nothing to surface.

## Public surface

```rust
pub enum Vendor { ClaudeCode }
pub enum Status { Working, WaitingApproval, WaitingPrompt, Idle }
pub struct AgentSession { /* vendor, id, path, cwd, status, first_seen_ms, … */ }
pub struct World { pub root: PathBuf, pub sessions: Vec<AgentSession> }
pub fn default_root() -> PathBuf;   // ~/.claude/projects
```

The format adapter (`agsess::claude`: `LineEvent`, `parse_line`, `parse_ts`) is
exposed for adapter-level tests but is **unstable vendor territory** — it may
churn with the transcript format and is anchored to fixtures, not a stable API.

## Develop

```
python dev.py check    # zero-dependency guard + cargo test (the pre-push gate)
python dev.py test     # cargo test
python dev.py fmt      # cargo fmt --check
python dev.py guard    # dependency guard (org crates only, no third-party)
```

## License

MIT — see [LICENSE](LICENSE). nativelite ships everything permissively.
