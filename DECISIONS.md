# Decisions

One line per decision, with the reason. Newest last.

- Two crates, `desk-core` (lib) and `desk` (CLI). The GUI will be a third crate, so egui never slows down the CLI that agents call.
- Few dependencies: anyhow, chrono, clap, ctrlc, serde, serde_json, toml. No async runtime: one thread per run is plenty for a handful of agents.
- **The supervisor polls (every 500ms) instead of using `notify`.** The workspace is a few dozen small files, polling behaves the same on every drive (network drives included), and a missed file event can't wedge it. Revisit only if it's measurably slow.
- `desk run` is the supervisor. Exactly one runs per workspace, enforced by a heartbeat file (`log/supervisor.pid`, stale after 5s), so the GUI and a terminal can't both deliver.
- Headers are flat `key: value`, not YAML. A small line-based parser keeps comments and key order when `desk task set` edits a field, and avoids a YAML dependency.
- `agent.toml` holds config (yours), and `state.toml` holds runtime state (session, state, context, errors), which only the supervisor writes. That way a human's edit and the supervisor never race on one file.
- CLI → supervisor requests are marker files (`agents/<n>/RESET`, `RETRY`, `PAUSED`), so they're visible and work whether or not the supervisor is running yet.
- Every agent gets a prompt on stdin, never as a CLI argument. This avoids Windows' command-line length limit and `.cmd` argument quoting.
- OpenCode has no system-prompt flag, so its role prompt goes at the top of each new session's first message. Claude gets it with `--append-system-prompt-file`.
- Session ids come from the stream (Claude's `init` event, OpenCode's `sessionID`) instead of being generated, so both adapters work the same way.
- `Adapter::parse` returns `Vec<Event>`, not `Option`, because one Claude line can hold text, tool calls and usage.
- A failed run retries after 30s, 60s, then stops at 3 failures and messages the human. Unread mail stays unread, so nothing is lost. `desk resume <agent>` retries.
- Locks are released when their agent is killed, reset, or stops after failures. They aren't released at the end of every run, because one lock (a dev server) often spans several turns.
- Unblocking (an answered ask) restores the status from before the block, read from the event log, instead of guessing `working`.
- The message budget counts agent-to-agent messages on a task, from `log/events.jsonl`. Messages to and from the human never count.
- The GUI is its own binary, `desk-gui`. If no supervisor is running it starts one in a thread, and closing the window stops it cleanly.
- The GUI uses eframe's `glow` (OpenGL) backend. The default `wgpu` backend segfaulted on startup on this machine, and glow is lighter anyway.
- egui's bundled fonts lack symbols like ● ⚙ →, so the GUI falls back to the OS symbol font (Segoe UI Symbol on Windows).
- Take-over v1 is a real terminal, not an embedded one. `HELD` stops delivery (and stops any current run, since two writers would corrupt one session), and the GUI opens `claude --resume` / `opencode --session` in Windows Terminal. The embedded `egui_term` pane stays on the roadmap.
- `desk send` accepts messages with hyphens (`a -> b`), and flags written after the message still count as flags. Agents ran into both.
