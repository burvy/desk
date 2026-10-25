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
- **Every CLI is a preset** (a TOML file: command, session handling, output format), and Claude and OpenCode are presets too. Adding a CLI needs no Rust unless it has a JSON stream worth parsing. Built-ins ship in the binary; `presets/<name>.toml` in a workspace overrides or adds one.
- Presets with plain-text output can't report a session id. They use `named` sessions when the CLI lets desk pick the name (goose), or `none`, where every run is fresh and memory.md carries the agent.
- A CLI that only takes its prompt as an argument (Cline) gets a one-line pointer to a prompt file instead. Long prompts as arguments break on Windows' command-line limit and `.cmd` quoting.
- The Claude preset removes `ANTHROPIC_API_KEY` so agents use your claude.ai login. A key in the environment silently takes precedence over the login, and the key here had no credit. Override the preset to bill a key.
- **Raw messages** (`desk send --raw`) go to the CLI alone and verbatim, as their own run. That lets the orchestrator (or you) run CLI commands like `/compact` without desk knowing about any of them. Claude Code runs `/compact` headless; that was checked. `desk reset` stays the portable way.
- `desk send --raw` refuses text that Git Bash has turned into a path (`/compact` → `C:/Program Files/Git/compact`) and says to pipe it in. Agents shell out through Git Bash, so they'd hit this.
- The OpenCode parser takes the session id from any event, not just `step_start`, which OpenCode doesn't always send. Missing it silently started a new session each run.
- desk-gui is a window app (`windows_subsystem = "windows"`): no console behind it. It opens the last workspace, or a start screen to open or create one, so it can be pinned and launched like any app. The recent list is in `%APPDATA%\desk\recent.txt`.
- Agents can be added and edited in the window (the same `agent::spawn` as `desk spawn`). Folder pickers use `rfd`, the one new dependency.
- **CLIs are edited in the window** (the CLIs view), as their TOML. Saving checks the preset, then writes `presets/<name>.toml`; an edited built-in can be reverted. A form with one field per setting would hide fields and need updating whenever presets change; the TOML is short and documented.
- **Terminal** opens a real terminal in the agent's project with desk's environment (workspace, agent name, `desk` first on PATH) and prints the exact command of its next run, which you can paste and run. It's for when something goes wrong. An embedded terminal was rejected for now (a big dependency, uncertain on Windows). `desk command <agent>` prints the same line.
- desk now always saves the latest prompt to `agents/<n>/scratch/prompt.md`, so a run can be replayed by hand (`... < prompt.md`).
- The orchestrator prompt now says when to add agents (independent tasks, nobody free), to tell the human, and to kill them when their group is done. `desk spawn` never had a restriction.
