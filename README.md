# desk

A small, transparent harness for a team of coding agents. Agents message each other through plain files. desk delivers the messages and runs the agents headless.

```
cargo install --path crates/desk

desk init myteam --human you && cd myteam
desk spawn mike --cli claude   --model sonnet                      --dir ~/code/game --role orchestrator
desk spawn rook --cli opencode --model opencode/glm-5.3-flash-free --dir ~/code/game
desk run                                   # the supervisor; leave it open
desk send mike "Explosions shouldn't level the world. Make blocks turn to sand."
desk status                                # agents, tasks, asks, locks
desk peek rook                             # what rook is doing
desk inbox                                 # what the agents told you
desk answer 3 "falling block"              # answer an agent's question
```

Everything lives in the workspace folder. `cat` it, edit it, commit it. `DESIGN.md` explains how it works, and `DECISIONS.md` explains why.

Supported CLIs: `claude` (Claude Code) and `opencode` (any provider OpenCode serves).
