# desk — design

A small, transparent harness for running a team of coding agents on one machine.
The orchestrator plans and merges; workers implement; everything they do is plain files you can read.

Status: v0 (core + CLI) works. v1 GUI (`desk-gui`) works: start screen, agents (add and edit), transcripts, task board, asks, inbox, composer, take-over.

## Principles

1. **Core first.** The core and CLI must work with no GUI. The GUI is a view.
2. **Files are the source of truth.** Every message, task, memory and decision is a file you can `cat`, grep, edit, and commit.
3. **Nothing is opaque.** Every message, tool call and file edit an agent makes is visible.
4. **Intent travels with the work.** Every task says *why*. The approach may change; the intent may not.
5. **Finish well, not fast.** The goal is robust structure, not an empty queue.
6. **Agents push back.** On structure, scope and irreversible choices they say what they'd do and why. When options are genuinely close they lay them out. Small, reversible things they just do.
7. **Teach the human the codebase.** The goal is a human who could work on the code themselves, and gives better direction because of it.
8. **Empower, don't restrict.** Agents stay on track because they know the intent and what "done" means, not because of rules. Give the why and the goal; trust them with the how. No sandboxing. Coordination happens through conventions (owned files, locks).

## Architecture

```
workspace folder            ← plain files, source of truth
      ▲
desk core (Rust lib)        ← watches inboxes, delivers, spawns/kills, logs
      │   └─ CLI presets: claude, opencode, codex, gemini, qwen, goose, cline, yours
      ├─ desk CLI           ← what agents (and you) call
      └─ desk GUI (egui)    ← what you look at
```

The supervisor (`desk run`) polls the workspace twice a second. Polling is cheap at this size and behaves the same on every drive (see DECISIONS.md). Only one supervisor runs per workspace.

## Workspace layout

```
workspace/
  desk.toml                 projects (name → path), default models, limits
  DESIGN.md                 the project's architecture (orchestrator owns)
  DECISIONS.md              one line per decision + reason
  PLAN.md                   current focus + groups in order (orchestrator owns)
  CONVENTIONS.md            project taste: commits, naming, testing
  agents/<name>/
    agent.toml              cli, model, dir, role, permissions (yours to edit)
    state.toml              session id, state, context, errors (supervisor writes)
    memory.md               what the agent keeps across sessions
    handoff.md              written before each reset
    scratch/                throwaway files
    inbox/                  one file per message
      read/                 delivered messages move here
  tasks/<id>-<slug>/
    task.md                 header + spec
    reports/<agent>.md      one report per agent per task
  asks/<id>.md              questions for the human
  locks/<name>.toml         shared-resource locks
  models/<model>.md         notes + track record per model
  prompts/                  orchestrator.md, worker.md (edit to taste)
  presets/<cli>.toml        your own CLI presets, or overrides of built-in ones
  log/
    events.jsonl            everything the core did
    transcripts/<agent>/<session>.jsonl
```

The human is a participant named in `desk.toml` and has an inbox like any agent.

## File formats

**Message** (`agents/rook/inbox/2026-10-25T1412-michael.md`)
```markdown
---
from: michael
to: rook
task: 014          # optional
kind: note         # note | interrupt | raw
---
Review the diff on branch attack-rework. Focus on the tick-order change.
```

**Task** (`tasks/014-explosion-sand/task.md`)
```markdown
---
id: 014
status: open
owner: rook
group: explosions  # related tasks share a group
tier: cheap        # cheap | strong
depends_on: []
files: [src/explosion.rs]
budget: 20         # max messages before the loop breaker trips
---
## Intent
Explosions shouldn't cause massive world destruction.

## Proposed approach
Exploded blocks turn into sand instead of being destroyed.

## Done when
No block type is destroyed outright by an explosion, including modded blocks.

## Non-goals
Changing explosion damage to entities.
```

Intent and Done when are required. A worker that receives a task without them asks before starting.

**Ask** (`asks/007.md`)
```markdown
---
from: rook
task: 014
blocking: true
status: open       # open | answered
---
Should sand drop as an item or stay as a falling block?
```

## Task lifecycle

```
proposed → open → assigned → working → review → done
                     ↘ blocked ↗          ↘ (sent back) → working
any state → cancelled
```

| Transition | Who |
|---|---|
| → proposed | anyone (including asides) |
| proposed → open | orchestrator, after the intent check |
| open → assigned | orchestrator (push) |
| assigned → working → review | owner |
| review → done / sent back | reviewer: never the owner, normally a stronger model or the orchestrator |
| → blocked | the owner, when it raises a blocking ask or needs a lock |

Header changes go through `desk task set` so two writers can't overwrite each other.

## Pacing

The orchestrator sets the pace, not the order ideas arrive in.

- New requests and asides land as `proposed`. The orchestrator acknowledges each one ("noted, grouped under X, after the current focus") instead of switching to it.
- It sorts related tasks into **groups** and finishes one group before starting the next. It doesn't multitask across unrelated work.
- `PLAN.md` records the current focus and the order of the groups that follow, so it survives resets and the human can see it. The human can reorder it or override the focus at any time.
- There's no fixed limit on how much runs at once. The orchestrator judges that per group.

**Keeping the orchestrator's load light:**
- **One task per turn.** The core groups the orchestrator's unread mail by `task:` and runs one turn per task. Each turn starts by re-reading that `task.md`, so nothing has to be remembered between turns.
- **Routine traffic skips it.** Reviewers send work back to owners directly, and workers coordinate small things with each other. The orchestrator gets decisions: new requests, finished reviews, blocked work, merges.

## Delivery

- An idle agent with unread mail gets one run. All unread messages are combined into one prompt (the orchestrator's are split by task, see Pacing).
- A message moves to `read/` only after the run exits cleanly. Delivery is at-least-once, so prompts say: "you may have seen some of this before; check memory first."
- `interrupt` messages stop the current run and resume with the interrupt first. `note` messages wait for the next turn.
- `raw` messages are a run of their own, and the CLI gets the text verbatim, with no desk framing. That's how to run a CLI's own commands, like `/compact` (Claude Code runs it headless).
- While a human has taken over an agent, delivery to it pauses.

## Asks

Agents ask the human through `asks/`, never buried in chat.
- `blocking: true`: the task goes to `blocked` and the agent stops spending tokens. The ask appears at the top of the Asks panel.
- `blocking: false`: the agent carries on, and you answer when you have time.

Answering an ask writes the answer into the ask file and messages the asker.

## Asides

Humans add thoughts on the spot, often in the middle of an answer. Every agent's rule: answer the question, then capture each aside as its own item (usually a `proposed` task or a DECISIONS.md line). Don't act on it inline, and don't drop it.

## Locks

`desk lock <name>` / `desk unlock <name>` for shared resources (a server, a port, a deploy). A lock records its holder and expires when that agent's process dies. The GUI shows them.

## Memory, reset and handoff

Before any reset the core runs one extra turn: *"Update memory.md and write handoff.md."* The new session starts with both. This works the same for every CLI, so desk doesn't depend on each CLI's own compaction.

The core reads token usage from the stream and warns when an agent's context is getting full. The orchestrator can `desk peek <agent>` (the end of the transcript) and `desk reset <agent>`.

## Review

Every task passes through `review` before `done`. The reviewer checks the work against Intent and Done when, not just "does it compile." The result is recorded in the owner model's track record.

## Models

A model is identified by `<cli>/<provider>/<model>` (for example `opencode/zai/glm-4.6`), because one CLI like OpenCode or Cline can serve many models. `models/<cli>-<provider>-<model>.md` holds the orchestrator's notes on each model (strengths, weaknesses) and a track record (passed or sent back), updated automatically from review results. The orchestrator picks models with this file and the task's `tier`. An unknown model gets a small calibration task first.

If a provider is rate-limited or down, the agent falls back to the next model listed in `desk.toml`.

## Safeguards

- A cap on how many agents run at once (`desk.toml`).
- A global pause.
- A per-task message budget. When it's exceeded, the task goes to `blocked` and an ask goes to the human. This mostly catches small models bouncing messages back and forth.

## Agent prompts

Two files, treated as source code: `orchestrator.md` and `worker.md`. Each has three layers:
- **Mechanics:** exact facts about desk (commands, paths). Precise and short.
- **Judgment:** how to think. A few sentences each, always with the reason.
- **Conventions:** not in the prompt. They point to the workspace's `CONVENTIONS.md`.

The orchestrator also:
- **On onboarding:** reads the code, drafts the project's DESIGN.md, and asks the human what it couldn't infer.
- **Per request:** restates the human's intent in its own words and names anything in the code that conflicts with it, before writing tasks. It fits the approach to what's best for the code, but never changes the intent.
- **After merges:** writes a short note on what changed and how that part works now.
- **Treats catching the human's mistakes as part of the job.** The human often can't see the code they're asking for.

## CLI

```
desk init [path] [--human name]    create a workspace
desk run                           the supervisor: delivers mail, runs agents
desk send <to> [--task N] [--interrupt | --raw] <msg>
desk spawn <name> --cli <preset> [--model m] --dir <path> --role <file>
desk presets                       the CLIs agents can use, and which are installed
desk kill <name>
desk reset <name>
desk peek <name>
desk command <name> [--script]     the command desk runs for it, to run by hand
desk status
desk task new | set <id> <field> <value>
desk ask <msg> [--blocking] [--task N]
desk answer <id> <msg>
desk inbox                         your unread messages
desk lock | unlock <name>
desk pause [--now] | resume [name] resume <name> retries a stopped agent
```

## CLI presets

Each agent CLI is a preset: a small TOML file that names the command, how sessions continue, and how to read the output. Claude and OpenCode are presets like the rest. Built-in presets ship in the binary (`crates/desk-core/presets/`), and a workspace's `presets/<name>.toml` adds or overrides one. `presets/README.md` lists the fields.

```toml
program = "goose"
args = ["run", "--instructions", "-", "--name", "{session}"]
model_args = ["--model", "{model}"]
session = "named"        # stream (id read from output) | named (desk names it) | none
resume = ["--resume"]
output = "text"          # claude | opencode (JSON streams desk parses) | text
```

The prompt goes to stdin unless `args` mention `{prompt_file}`. The only CLI-specific Rust is the two stream parsers, which give desk tool calls, token counts and session ids. With `text` output, the transcript is the CLI's plain output. Claude and OpenCode are verified; the others are written from each CLI's `--help` and marked untested.

## GUI (egui)

- Opens the last workspace, or a start screen: recent workspaces, open one, create one.
- Left: agents with state (idle, running, blocked, error) and model. Add an agent, or edit one's settings, in a form.
- Center: the selected agent's live transcript, or the selected task.
- Right: the task board by status.
- Top: the Asks panel. Blocking asks come first.
- Bottom: message any agent (note, interrupt or raw).
- CLIs: edit each preset's TOML, add your own, revert edits.
- Terminal: a real terminal in the agent's project with desk's environment and its next command printed.
- Pause-all, and take-over: holds the agent's mail and opens its session in a real terminal (`desk take-over` / `hand-back` from the CLI). Later: an embedded `egui_term` pane instead.

## Roadmap

1. **v0:** core + CLI, with Claude and OpenCode agents messaging each other.
2. **v1:** the GUI.
3. **First real use:** review a large project built in 3 days. Fix what that reveals.
4. **Later:** embedded take-over terminals, model calibration, fallbacks, leads (an agent with an orchestrator-style prompt that coordinates one large group).

## Permissions and git

- **Agents run with full permissions** (Claude: skip-permissions; OpenCode: its equivalent). It's set per agent in `agent.toml` (`permissions = "bypass" | "default"`), so one agent can be tightened without touching the rest. Git is the safety net: the orchestrator makes sure work is committed before a task starts.
- **Commits belong to the human.** Agents commit under the human's git identity with no AI co-author lines. The orchestrator's notes after each merge exist so the human understands what they're pushing and owns it.

## Not doing

Memory search, integrations, cost dashboards, merge tooling, cross-machine sync, sandboxing, worktrees.

## Settled

- **The GUI writes through the core library,** the same code path the CLI uses, never raw file writes. One place handles atomic writes and validation.
- **Git branching is flexible.** desk doesn't care. The project's CONVENTIONS.md decides, and the orchestrator follows it.
- **Push only.** Pool tasks (idle workers claiming open tasks) are dropped. Revisit if the orchestrator becomes a bottleneck.

## Open questions

- The defaults `desk init` writes (models, limits, prompts).
