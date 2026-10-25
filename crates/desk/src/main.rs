//! `desk`: the command line. What agents call, and what you call.
//! Every command is a thin wrapper over desk-core.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use desk_core::adapter::{self, Event, Preset};
use desk_core::agent::{self, AgentConfig, AgentState};
use desk_core::message::{self, Kind};
use desk_core::supervisor::{self, Supervisor};
use desk_core::task::{self, NewTask};
use desk_core::transcript::{self, Entry};
use desk_core::{Workspace, ask, lock, models};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "desk", version, about = "A small, transparent harness for a team of coding agents.")]
struct Cli {
    /// The workspace folder. Default: $DESK_WORKSPACE, or the nearest desk.toml above here.
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a workspace.
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Your name in the workspace.
        #[arg(long, default_value = "human")]
        human: String,
    },
    /// Run the supervisor: deliver mail, start and stop agents. Ctrl-C to stop.
    Run,
    /// Message an agent or the human. `-` reads the message from stdin.
    Send {
        to: String,
        #[arg(long)]
        task: Option<String>,
        /// Stop the recipient's current run and deliver this first.
        #[arg(long)]
        interrupt: bool,
        /// Deliver the text alone and verbatim, as its own run (e.g. `/compact`).
        #[arg(long)]
        raw: bool,
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        message: Vec<String>,
    },
    /// Show your unread messages and mark them read.
    Inbox {
        /// Leave them unread.
        #[arg(long)]
        keep: bool,
    },
    /// Create an agent, or reactivate one with new settings. Memory is kept.
    Spawn {
        name: String,
        /// A CLI preset (`desk presets` lists them).
        #[arg(long)]
        cli: String,
        /// Whatever the CLI accepts. Empty: the CLI's own default.
        #[arg(long, default_value = "")]
        model: String,
        /// The project folder, or a project name from desk.toml.
        #[arg(long)]
        dir: String,
        /// orchestrator, worker, or a prompt file.
        #[arg(long, default_value = "worker")]
        role: String,
        /// bypass (no permission prompts) or default.
        #[arg(long, default_value = "bypass")]
        permissions: String,
    },
    /// List the CLI presets agents can use, and whether each CLI is installed.
    Presets,
    /// Stop an agent and release its locks. Its files are kept.
    Kill {
        name: String,
    },
    /// Have an agent write its handoff, then give it a fresh session.
    Reset {
        name: String,
    },
    /// Hold an agent's mail and print the command that opens its session for you.
    TakeOver {
        name: String,
    },
    /// End a take-over; held mail is delivered.
    HandBack {
        name: String,
    },
    /// Show the end of an agent's transcript.
    Peek {
        name: String,
        #[arg(short, default_value_t = 30)]
        n: usize,
    },
    /// Agents, tasks, asks and locks at a glance.
    Status,
    #[command(subcommand)]
    Task(TaskCmd),
    /// Ask the human a question. `-` reads it from stdin.
    Ask {
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        question: Vec<String>,
        /// Block the task until answered.
        #[arg(long)]
        blocking: bool,
        #[arg(long)]
        task: Option<String>,
    },
    /// Answer an ask. The asker gets the answer as a message.
    Answer {
        id: String,
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        answer: Vec<String>,
    },
    /// Take a lock on a shared resource.
    Lock {
        name: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    Unlock {
        name: String,
    },
    /// Stop starting new runs. --now also stops runs in progress (their mail stays unread).
    Pause {
        #[arg(long)]
        now: bool,
    },
    /// Undo pause, or with a name, retry an agent that stopped after failures.
    Resume {
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Create a task (status: proposed unless --status).
    New {
        title: String,
        #[arg(long, default_value = "")]
        intent: String,
        #[arg(long, default_value = "")]
        done_when: String,
        #[arg(long, default_value = "")]
        approach: String,
        #[arg(long, default_value = "")]
        non_goals: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        tier: Option<String>,
        #[arg(long, value_delimiter = ',')]
        files: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        depends_on: Vec<String>,
        #[arg(long)]
        budget: Option<u32>,
    },
    /// Change one header field: `desk task set 14 status review`.
    Set { id: String, field: String, value: String },
    /// Print a task.
    Show { id: String },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("desk: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    if let Cmd::Init { path, human } = &cli.cmd {
        let ws = Workspace::init(path, human)?;
        println!("workspace ready at {}", ws.root.display());
        println!(
            "next: `desk spawn <name> --cli opencode --model <m> --dir <project> --role orchestrator`, then `desk run`"
        );
        return Ok(());
    }
    let ws = match &cli.workspace {
        Some(p) => Workspace::open(p)?,
        None => Workspace::find(&std::env::current_dir()?)?,
    };
    let me = ws.caller();

    match cli.cmd {
        Cmd::Init { .. } => unreachable!(),
        Cmd::Run => supervise(ws)?,
        Cmd::Send { to, task, interrupt, raw, mut message } => {
            let task = task.or(take_value(&mut message, "--task"));
            let interrupt = interrupt | take_flag(&mut message, "--interrupt");
            let raw = raw | take_flag(&mut message, "--raw");
            let kind = match (raw, interrupt) {
                (true, _) => Kind::Raw,
                (_, true) => Kind::Interrupt,
                _ => Kind::Note,
            };
            let body = text(message)?;
            // Git Bash rewrites a lone `/compact` argument into `C:/Program Files/Git/compact`.
            if raw && std::env::var_os("MSYSTEM").is_some() && std::path::Path::new(body.trim()).is_absolute()
            {
                bail!(
                    "Git Bash turned the message into a path ({body}). Pass it on stdin instead: `echo /compact | desk send {to} --raw -`"
                );
            }
            let path = message::send(&ws, &me, &to, task.as_deref(), kind, &body)?;
            println!("sent to {to} ({})", path.display());
            if !supervisor::is_alive(&ws) && to != ws.config.human {
                eprintln!("note: no supervisor is running, so it waits until `desk run`");
            }
        }
        Cmd::Inbox { keep } => {
            let unread = message::unread(&ws, &me);
            if unread.is_empty() {
                println!("no unread messages");
            }
            for m in &unread {
                let task = m.task.as_ref().map(|t| format!(" · task {t}")).unwrap_or_default();
                let kind = match m.kind {
                    Kind::Interrupt => " · INTERRUPT",
                    Kind::Raw => " · RAW",
                    Kind::Note => "",
                };
                println!("── from {}{task}{kind} ──\n{}\n", m.from, m.body.trim_end());
            }
            if !keep {
                let paths: Vec<PathBuf> = unread.iter().map(|m| m.path.clone()).collect();
                message::mark_read(&ws, &me, &paths)?;
            }
        }
        Cmd::Spawn { name, cli, model, dir, role, permissions } => {
            let dir = ws.resolve_dir(&dir)?;
            agent::spawn(&ws, &name, AgentConfig { cli, model, dir, role, permissions, active: true })?;
            println!("{name} is ready. It starts when it gets a message.");
        }
        Cmd::Presets => {
            for name in adapter::names(&ws) {
                match Preset::load(&ws, &name) {
                    Ok(p) => {
                        let found = if p.installed() { "" } else { "  (not installed)" };
                        println!("{name:<10} {}{found}", p.description);
                    }
                    Err(e) => println!("{name:<10} broken: {e:#}"),
                }
            }
            println!(
                "
Add or override one with presets/<name>.toml in the workspace."
            );
        }
        Cmd::Kill { name } => {
            agent::kill(&ws, &name)?;
            println!("{name} deactivated; `desk spawn` brings it back with its memory");
        }
        Cmd::TakeOver { name } => {
            let cmd = agent::take_over(&ws, &name)?;
            let dir = AgentConfig::load(&ws, &name)?.dir;
            println!("{name}'s mail is held. Its session, in {}:\n\n  {}\n", dir.display(), cmd.join(" "));
            println!("`desk hand-back {name}` when you're done.");
        }
        Cmd::HandBack { name } => {
            agent::hand_back(&ws, &name)?;
            println!("{name} is back with desk");
        }
        Cmd::Reset { name } => {
            agent::request_reset(&ws, &name)?;
            println!("{name} will write a handoff, then start fresh");
        }
        Cmd::Peek { name, n } => peek(&ws, &name, n)?,
        Cmd::Status => status(&ws),
        Cmd::Task(t) => task_cmd(&ws, t)?,
        Cmd::Ask { mut question, blocking, task } => {
            let task = task.or(take_value(&mut question, "--task"));
            let blocking = blocking | take_flag(&mut question, "--blocking");
            let a = ask::new(&ws, &me, task.as_deref(), blocking, &text(question)?)?;
            println!(
                "ask {} raised{}",
                a.id,
                if blocking { "; the task is blocked until it's answered" } else { "" }
            );
        }
        Cmd::Answer { id, answer } => {
            let a = ask::answer(&ws, &id, &text(answer)?)?;
            println!("answered ask {}", a.id);
        }
        Cmd::Lock { name, note } => {
            lock::acquire(&ws, &name, &me, &note)?;
            println!("locked {name}");
        }
        Cmd::Unlock { name } => {
            lock::release(&ws, &name, &me)?;
            println!("unlocked {name}");
        }
        Cmd::Pause { now } => {
            std::fs::write(ws.paused_flag(), if now { "now" } else { "" })?;
            ws.log("pause", serde_json::json!({ "now": now }));
            println!(
                "paused{}",
                if now { "; running agents are being stopped" } else { "; running agents finish their turn" }
            );
        }
        Cmd::Resume { name: None } => {
            let _ = std::fs::remove_file(ws.paused_flag());
            ws.log("resume", serde_json::json!({}));
            println!("resumed");
        }
        Cmd::Resume { name: Some(name) } => {
            agent::request_retry(&ws, &name)?;
            println!("{name} will retry");
        }
    }
    Ok(())
}

/// Messages may contain hyphens (`a -> b`), so clap takes every word after
/// the first as text. A flag given after the message arrives as its own
/// word, though, so pull those back out rather than send them as text.
fn take_flag(words: &mut Vec<String>, flag: &str) -> bool {
    let before = words.len();
    words.retain(|w| w != flag);
    words.len() != before
}

fn take_value(words: &mut Vec<String>, flag: &str) -> Option<String> {
    let i = words.iter().position(|w| w == flag)?;
    if i + 1 >= words.len() {
        return None;
    }
    let value = words.remove(i + 1);
    words.remove(i);
    Some(value)
}

/// Joins the words of a message; `-` reads stdin.
fn text(words: Vec<String>) -> Result<String> {
    if words.len() == 1 && words[0] == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(s);
    }
    Ok(words.join(" "))
}

fn supervise(ws: Workspace) -> Result<()> {
    let mut sup = Supervisor::start(ws)?;
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    // Ctrl-C sets a flag so the agents are stopped cleanly, not orphaned.
    ctrlc::set_handler(move || s.store(true, Ordering::SeqCst))?;
    println!("supervising {} (ctrl-c to stop)", sup.workspace().root.display());
    while !stop.load(Ordering::SeqCst) {
        for note in sup.tick() {
            println!("{} {note}", chrono_now());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("stopping…");
    sup.shutdown();
    Ok(())
}

fn chrono_now() -> String {
    desk_core::fsutil::now_rfc3339().get(11..19).unwrap_or("").to_string()
}

fn peek(ws: &Workspace, name: &str, n: usize) -> Result<()> {
    let cfg = AgentConfig::load(ws, name)?;
    let preset = Preset::load(ws, &cfg.cli)?;
    let Some(path) = transcript::latest(ws, name) else { bail!("{name} has no transcript yet") };
    let entries: Vec<Entry> = transcript::entries(&preset, &path)
        .into_iter()
        .filter(|e| !matches!(e, Entry::Event(Event::Usage { .. })))
        .collect();
    for e in &entries[entries.len().saturating_sub(n)..] {
        println!("{}", transcript::render(e));
    }
    Ok(())
}

fn status(ws: &Workspace) {
    let sup = if supervisor::is_alive(ws) { "running" } else { "NOT running (`desk run`)" };
    let paused = if ws.is_paused() { " · PAUSED" } else { "" };
    println!("supervisor {sup}{paused}\n");

    println!("AGENTS");
    for name in agent::names(ws) {
        let Ok(cfg) = AgentConfig::load(ws, &name) else { continue };
        let st = AgentState::load(ws, &name);
        let state = match st.state.as_str() {
            _ if agent::is_held(ws, &name) => "held",
            "" => "idle",
            s => s,
        };
        let unread = message::unread(ws, &name).len();
        let (pass, back) = models::tally(ws, &cfg);
        let mut line = format!(
            "  {name:<12} {state:<9} {:<12} {}/{}  ctx {}k  unread {unread}  record {pass}✓ {back}↩",
            cfg.role,
            cfg.cli,
            cfg.model,
            st.context_tokens / 1000
        );
        if let Some(e) = &st.last_error {
            line.push_str(&format!("\n               last error: {}", adapter::clip(e, 100)));
        }
        println!("{line}");
    }

    println!("\nTASKS");
    for t in task::list(ws).iter().filter(|t| !matches!(t.status().as_str(), "done" | "cancelled")) {
        let missing = t.missing_sections();
        let warn =
            if missing.is_empty() { String::new() } else { format!("  ⚠ missing {}", missing.join(", ")) };
        println!(
            "  {} {:<9} {:<12} {:<12} {}{warn}",
            t.id,
            t.status(),
            t.owner().unwrap_or_else(|| "-".into()),
            t.group().unwrap_or_else(|| "-".into()),
            t.title()
        );
    }
    let done = task::list(ws).iter().filter(|t| t.status() == "done").count();
    if done > 0 {
        println!("  ({done} done)");
    }

    let asks = ask::open(ws);
    if !asks.is_empty() {
        println!("\nASKS");
        for a in asks {
            let task = a.task().map(|t| format!(" task {t}")).unwrap_or_default();
            let b = if a.blocking() { "BLOCKING " } else { "" };
            println!("  {} {b}from {}{task}: {}", a.id, a.from(), adapter::clip(a.question(), 90));
        }
    }
    let locks = lock::list(ws);
    if !locks.is_empty() {
        println!("\nLOCKS");
        for l in locks {
            println!("  {:<12} {} since {} {}", l.name, l.holder, l.since, l.note);
        }
    }
    let mine = message::unread(ws, &ws.config.human).len();
    if mine > 0 {
        println!("\n{mine} unread for {} (`desk inbox`)", ws.config.human);
    }
}

fn task_cmd(ws: &Workspace, t: TaskCmd) -> Result<()> {
    match t {
        TaskCmd::New {
            title,
            intent,
            done_when,
            approach,
            non_goals,
            status,
            owner,
            group,
            tier,
            files,
            depends_on,
            budget,
        } => {
            let t = task::new(
                ws,
                NewTask {
                    title,
                    intent,
                    done_when,
                    approach,
                    non_goals,
                    status,
                    owner,
                    group,
                    tier,
                    files,
                    depends_on,
                    budget,
                },
            )?;
            println!("task {} created: {}", t.id, t.path().display());
            let missing = t.missing_sections();
            if !missing.is_empty() {
                println!("fill in before it can start: {}", missing.join(", "));
            }
        }
        TaskCmd::Set { id, field, value } => {
            let t = task::set(ws, &id, &field, &value)?;
            println!("task {}: {field} = {value}", t.id);
        }
        TaskCmd::Show { id } => {
            let t = task::Task::find(ws, &id)?;
            print!("{}", t.doc.render());
        }
    }
    Ok(())
}
