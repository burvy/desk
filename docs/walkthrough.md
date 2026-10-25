# Walkthrough

How desk works, file by file, so you can own it. About 3,700 lines of Rust.

## The one idea

The supervisor owns every agent process. That's what your old setup couldn't do: a process can only type into a terminal it created. desk never types into terminals. Each agent turn is one headless run (`claude -p …` / `opencode run …`). The prompt goes to stdin and a JSON stream comes back. Between runs, the agent is just files.

## Follow one message

1. You run `desk send rook "fix the sand"`. **`message.rs` → `send`** writes `agents/rook/inbox/<timestamp>-you.md`: a `---` header (`from/to/task/kind`), then the body. It appends a `sent` event to `log/events.jsonl`.
2. **`supervisor.rs` → `tick`** runs twice a second (from `desk run`, or a thread inside `desk-gui`). For each agent, `step` checks whether it's inactive, already running, held, paused, failing or being reset. If none of those apply and it has unread mail, it starts a run.
3. `pick_batch` chooses the mail: interrupts first, then everything else. An orchestrator gets one task's mail per turn.
4. **`prompt.rs` → `delivery`** writes the prompt: who you are, project vs workspace, the human's name, an at-least-once reminder, then the messages. OpenCode sessions also get the role prompt on top, because OpenCode has no system-prompt flag.
5. `launch` builds the command from the agent's **preset** (`adapter/mod.rs` reads `presets/<cli>.toml`). It sets `DESK_WORKSPACE`, `DESK_AGENT` and `PWD`, and puts the `desk` binary first on PATH. Then it spawns the process. A thread writes the prompt to stdin. Another thread reads stdout line by line: it writes each raw line to `log/transcripts/rook/<session>.jsonl` and sends the parsed `Event`s (from `adapter/claude.rs` or `adapter/opencode.rs`, or plain text lines; session id, text, tool calls, token usage, errors) back to the supervisor.
6. When the process exits, `on_exit` decides:
   - **Clean:** the delivered messages move to `inbox/read/`.
   - **Stopped on purpose** (interrupt, kill, pause, take-over): nothing moves, so the mail is redelivered.
   - **Failed:** retry after 30s, then 60s, then stop and message you.
7. Inside its run, rook calls `desk send mike …`, `desk task set 14 status review` and so on. The CLI (`crates/desk/src/main.rs`) is a thin layer over the same core functions.

## The files

| File | What it is |
|---|---|
| `frontmatter.rs` | The `---` header format. Line-based, so `set` keeps your comments and key order. |
| `fsutil.rs` | Atomic writes (temp file + rename), `create_new`, a cross-process `FileLock`, ids, slugs. |
| `workspace.rs` | Finds the workspace, reads `desk.toml`, knows every path, and writes `events.jsonl`. `init` lays out a new workspace. |
| `agent.rs` | `agent.toml` (your config) vs `state.toml` (written only by the supervisor). Spawn, kill, reset, take-over. |
| `message.rs` | Send, unread and mark-read, plus the budget breaker. |
| `task.rs` | Tasks, `## sections`, `set` (the only header writer), and review verdicts feeding `models/`. |
| `ask.rs` | Asks. A blocking ask blocks its task. Answering restores the status from before the block and messages the asker. |
| `lock.rs` | `locks/<name>.toml`. Released on kill, reset, or repeated failure. |
| `models.rs` | `models/<cli>-<model>.md`: notes plus an automatic track record. |
| `prompt.rs` | Every sentence desk itself says to agents, in one place. |
| `transcript.rs` | Reads transcripts back for `peek` and the GUI. |
| `supervisor.rs` | The loop above. The only code that starts or stops processes. |
| `adapter/*` | `mod.rs`: presets, which turn an agent into a command line. `claude.rs`, `opencode.rs`: their JSON stream parsers. |
| `presets/*.toml` | The built-in CLI presets. Adding a CLI usually means adding one of these, no Rust. |
| `prompts/*.md` | **Drafts** of the orchestrator and worker prompts. `desk init` copies them into each workspace's `prompts/`, where you can edit them. |
| `desk-gui/*` | `main.rs` picks the workspace at startup. `home.rs` is the start screen. `snapshot.rs` reads the workspace about once a second. `app.rs` draws it. `agent_form.rs` adds or edits an agent. Every button calls the same core functions as the CLI. |

## Tested tonight (free OpenCode models only)

- **Full loop:** you → orchestrator → task → worker → review → done → track record → a note to you. The orchestrator also caught a real mistake: a file in the wrong folder.
- **Mail handling:** interrupt mid-run (the run is killed and restarted with the interrupt first). The budget breaker blocks the task and raises an ask. A bad model fails 3 times, then stops and messages you.
- **Lifecycle commands:** reset with handoff (memory.md and handoff.md written, new session), kill and respawn, `pause --now` and resume.
- **GUI:** live transcript, task view, and a clean supervisor shutdown when the window closes.

Bugs found by running it, all fixed:
- Agents worked in the workspace instead of the project (OpenCode trusts an inherited `$PWD`).
- `desk send` broke on messages containing `->`, and swallowed flags written after the message.
- Model notes were split across two files.
- Answering an ask always unblocked the task to `working`, even when it had been `open`.

## Not verified

- **Claude agents never completed a real run.** Your environment sets `ANTHROPIC_API_KEY` (the CLI said so), and that key has no credit, so Claude agents will fail with "Credit balance is too low". To use your subscription instead, unset `ANTHROPIC_API_KEY` in the shell that starts `desk run` / `desk-gui`. The adapter's flags are checked against `claude --help`, and the stream parser is tested against the real stream format.
- The **Take over** button opens Windows Terminal (`wt`), falling back to `cmd`. I didn't click it, because it would pop up a real interactive session.

## Where I'd push back or want your call

- **The prompts are drafts.** They're the part that most shapes behavior. Read `crates/desk-core/prompts/*.md`.
- **Full bypass** stays the default, as you chose. The real risk is an agent with full permissions reading untrusted content (web pages, downloads). `permissions = "default"` in one agent's `agent.toml` tightens just that agent.
- **I used polling instead of `notify`** (see DECISIONS.md). That's a deliberate change from DESIGN.md.

## Added the next day: presets, raw messages, the start screen

- **Presets.** `agent.toml`'s `cli` names a preset. `Preset::command` fills the placeholders and adds `model_args`, `bypass`, `system` and `resume` as needed. Named sessions get their name in `launch`, before the first run.
- **Raw messages.** `pick_batch` makes a raw message a run of its own. `step` sends its text without `prompt::delivery`'s framing.
- **Tested:** `/compact` sent raw to a Claude agent compacted its session (34,644 → 8,311 tokens), on the claude.ai login with no API-key error. The window reopened the last workspace with no console behind it, showed the start screen when there wasn't one, and stopped its supervisor on close.
- **Bugs found:** OpenCode doesn't always send `step_start`, so desk sometimes lost the session id and started over. Git Bash rewrites `/compact` into a path.
- **Not verified:** the codex, gemini, qwen, goose and cline presets never ran, because their providers might cost money. The Add agent form was never clicked, because clicking means taking over your mouse. OpenCode given `/compact` raw wrote a summary, but whether it actually compacted is unclear.

## Then: editing CLIs, Terminal

- `clis.rs` (GUI) edits a preset's TOML, through `adapter::save`, `remove`, `origin` and `source` in core.
- `supervisor::command_line` builds an agent's next command with the same `agent_command` the supervisor uses, so what you see is what desk runs. `CommandLine::script` wraps it in a `.cmd` with desk's environment. Terminal writes that to `scratch/terminal.cmd` and opens it in Windows Terminal.
- **Tested:** the script ran in `cmd`. It set the environment, put `desk` first on PATH, and the printed command replayed rook's last prompt and continued its session. Saving, overriding, reverting and typo rejection are unit-tested.
- **Not clicked:** the CLIs view and the Terminal button (the same mouse reason as before). The zcode preset never ran.

## Then: terminal agents

- `terminal.rs` (core, `terminal` feature) runs a CLI in a pseudo-terminal through `alacritty_terminal`. It also types into it (`write`, `paste`) and reads its screen (`text`).
- `term_run.rs` is the supervisor's side. Each tick it hashes the screen, and once it has been quiet for 3s and shows the preset's `ready` text, it types the next delivery in. The registry it fills is how the GUI finds each terminal.
- `term_view.rs` (GUI) draws the screen cell by cell and turns keys, paste, clicks and scrolling into terminal input.
- **Tested:** OpenCode's TUI ran in the window. A message typed in by desk was done end to end: a file was created, `desk send` reported back, memory was updated, and the agent went back to idle. A raw `/compact` compacted that session. Claude Code's TUI renders; with its trust prompt up, desk left a message unread and marked the agent "waiting" instead of typing. Closing the window ended the whole CLI process tree.
- **Not tested:** typing into the terminal yourself (that needs your mouse and keyboard), a Claude turn in a terminal (blocked on the trust prompt, which needs a click), and zcode in a terminal.
