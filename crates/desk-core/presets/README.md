# Presets

How desk runs each agent CLI. Two are built in: claude and opencode, whose
output desk reads. Any other CLI is a command you type in the Add agent form
("Type a command…"), which saves a preset here that runs it in a terminal. A
file here, `<name>.toml`, adds a CLI or replaces a built-in one. `desk presets`
lists them all.

`per_project` is added once for each of the workspace's other project folders,
with `{project}` filled in (Claude: `["--add-dir", "{project}"]`).

An agent's `cli` in `agent.toml` names its preset.

```toml
description = "My CLI"
program = "mycli"                       # found on PATH (npm .cmd shims too)
args = ["run", "--json"]                # every headless run
model_args = ["--model", "{model}"]     # when the agent sets a model
bypass = ["--yes"]                      # when permissions = "bypass"
system = ["--system-file", "{role_file}"]  # omit: the role prompt starts the first message
session = "none"                        # stream (id read from output) | named (desk names it) | none
resume = ["--resume", "{session}"]      # when continuing a session
output = "text"                         # claude | opencode (their JSON streams) | text
interactive = ["mycli", "--model", "{model}"]  # take-over and terminal agents
env = { SOME_KEY = "" }                 # set variables; "" removes one
model_env = { MYCLI_MODEL = "{model}" } # set only when the agent sets a model
models = ["model-a"]                    # suggestions in the GUI
```

Placeholders: `{model}` `{session}` `{dir}` `{workspace}` `{role_file}` `{prompt_file}`.
The prompt goes to stdin, unless `args` mention `{prompt_file}`: then desk
writes it to that file and the CLI is pointed at it.

With `session = "none"` every run starts fresh, and the agent relies on its
memory.md. With `output = "text"` the transcript shows the CLI's plain output,
so tool calls and token counts only appear if the CLI prints them.

In `interactive`, an option whose value is `{model}` or `{session}` is left
out when that's empty. Terminal agents run `interactive` (plus `bypass` and
`system`) in a terminal inside desk-gui, and desk types their messages in.
`interrupt` is what stops a turn there (default: Escape). `ready` is text the
screen shows only when the CLI is waiting for a message (one string, or a list
where any will do); desk types only then, so it never answers a trust or
permission prompt for you.
