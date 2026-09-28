//! localforest: local dev infrastructure for many git worktrees at once, made for coding
//! agents (Claude Code) that give every task its own worktree.
//!
//! - worktrees are copy-on-write clones of the primary checkout (git-cow)
//! - one PostgreSQL on an APFS RAM disk; each worktree's database is a copy-on-write
//!   clone of a template kept in step with the base branch
//! - https://<worktree>.<project>.localhost per worktree, starting its server on demand
//! - one Redis port; the password picks the checkout's own redis-server
//! - branches follow GitHub (webhook websocket), migrated as they move; merged
//!   worktrees are removed
//! - a worktree-aware LSP proxy
//!
//! See README.md, `localforest --help` and devenv-module/devenv.nix.

mod client;
mod config;
mod daemon;
mod github;
mod history;
mod lsp;
mod pgproxy;
mod postgres;
mod proxy;
mod ramdisk;
mod redis;
mod server;
mod sync;
mod tls;
mod worktree;

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::config::{Global, Project, ProjectSettings};

#[derive(Parser)]
#[command(
    version,
    about = "Worktrees, databases, HTTPS hosts, Redis and LSP for parallel coding agents"
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (or register this project with the running one). For devenv's
    /// `processes`.
    Serve {
        #[command(flatten)]
        project: ProjectSettings,
        /// Any checkout of the project.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Print this checkout's environment (PORT, DATABASE_URL, REDIS_URL, ...) as
    /// shell exports: `eval "$(localforest env)"`.
    Env {
        #[command(flatten)]
        project: ProjectSettings,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// This service's environment [default: the default service's].
        #[arg(short, long)]
        service: Option<String>,
        /// Print JSON instead.
        #[arg(long)]
        json: bool,
    },
    /// Manage worktrees.
    #[command(subcommand)]
    Worktree(WorktreeCmd),
    /// Show projects, worktrees, URLs and databases.
    Status,
    /// Pull branches, merge the base branch into worktrees, remove merged worktrees,
    /// migrate and refresh the template now.
    Sync {
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Replace the template database with a copy of the primary checkout's database.
    Snapshot {
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Trust the local CA for https://*.localhost (macOS keychain; asks for your password).
    Trust,
    /// Stop the daemon; --eject also drops the RAM disk with every database.
    Down {
        #[arg(long)]
        eject: bool,
    },
    /// A checkout's services (`localforest.services`).
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Worktree-aware LSP proxy: localforest lsp -- <server> [args]
    Lsp {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Claude Code hooks (JSON on stdin).
    #[command(subcommand)]
    Hook(HookCmd),
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// Create a worktree on a new branch <name> from the base branch; prints its path.
    New {
        name: String,
        /// Start from this commit-ish instead of the fetched base branch.
        #[arg(long)]
        base: Option<String>,
    },
    /// Remove a worktree: SIGKILL everything running in it (server, BEAM, watchers),
    /// drop its databases and redis-server. Without --force only when nothing would be
    /// lost (clean, and merged or without own commits).
    Rm {
        /// Name or path.
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// List worktrees.
    List,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Start it and what it depends on, and wait until it listens.
    Start(ServiceArgs),
    /// Kill it (its whole process group).
    Stop(ServiceArgs),
    /// Restart it.
    Restart(ServiceArgs),
    /// Print its log file path.
    Log(ServiceArgs),
}

#[derive(clap::Args)]
struct ServiceArgs {
    /// Service [default: the default service].
    #[arg(short, long)]
    service: Option<String>,
    /// Worktree [default: the one you're in; `.` for the primary checkout].
    worktree: Option<String>,
}

#[derive(Subcommand)]
enum HookCmd {
    /// WorktreeCreate: {"name": ...} -> prints the path.
    WorktreeCreate,
    /// WorktreeRemove: {"worktree_path": ...}
    WorktreeRemove,
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn project_for(path: &Path, settings: ProjectSettings) -> Result<Project> {
    let root = config::primary_root(path)?;
    let mut p = Project::new(root, settings);
    p.env = std::env::vars().collect();
    Ok(p)
}

fn root_of(path: &Path) -> Result<PathBuf> {
    config::primary_root(path)
}

async fn create(root: PathBuf, name: String, base: Option<String>) -> Result<daemon::CreateResp> {
    client::post("/worktrees", &daemon::CreateReq { root, name, base }).await
}

async fn remove(root: PathBuf, name: String, force: bool) -> Result<()> {
    let keep = vec![std::process::id() as i32, unsafe { libc::getppid() }];
    client::post::<Value>(
        "/worktrees/remove",
        &daemon::RemoveReq {
            root,
            name,
            force,
            keep_pids: keep,
        },
    )
    .await?;
    Ok(())
}

fn print_status(s: &daemon::Status) {
    println!(
        "daemon {} · postgres 127.0.0.1:{} · redis 127.0.0.1:{} · https :{}",
        s.pid, s.pg_port, s.redis_port, s.https_port
    );
    for p in &s.projects {
        println!(
            "\n{} ({}) base {}{}",
            p.project.name,
            p.project.root.display(),
            p.base,
            p.github.as_ref().map_or(String::new(), |g| format!(
                " · {g} ({})",
                if p.websocket { "websocket" } else { "polling" }
            ))
        );
        for c in &p.checkouts {
            println!(
                "  {:<24} {:<40} {} [{}]{}",
                c.checkout.worktree.as_deref().unwrap_or("(primary)"),
                c.url,
                c.branch.as_deref().unwrap_or("-"),
                c.databases.join(", "),
                if c.redis { " redis" } else { "" },
            );
            for s in &c.services {
                println!(
                    "    {:<20} :{:<5} {:<45} {}",
                    s.name,
                    s.port,
                    s.url.as_deref().unwrap_or("(no http)"),
                    if s.running { "running" } else { "-" }
                );
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "localforest=info".into()),
        )
        .init();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;

    match cli.command {
        Cmd::Serve { project, path } => {
            let p = project_for(&path, project)?;
            daemon::serve(cli.global, Some(p)).await
        }
        Cmd::Env {
            project,
            path,
            service,
            json,
        } => {
            let (root, wt, co_path) = config::locate(&path)?;
            let p = Project::new(root, project);
            // The port the daemon records (or will): same plan over the same projects.
            let port = if wt.is_some() && worktree::recorded_port(&co_path).is_none() {
                let mut all = daemon::registered_projects();
                all.retain(|o| o.root != p.root);
                all.push(p.clone());
                worktree::plan_ports(&all)
                    .ok()
                    .and_then(|plan| plan.into_iter().find(|(path, ..)| *path == co_path))
                    .map(|(_, port, _)| port)
            } else {
                None
            };
            let c = match port {
                Some(port) => p.checkout_on(wt.as_deref(), co_path, port),
                None => p.checkout(wt.as_deref(), co_path),
            };
            let env = match service.as_deref() {
                Some(s) if c.service(s).is_none() => anyhow::bail!("no service {s}"),
                Some(s) => c.service_env(&cli.global, Some(s)),
                None => c.env(&cli.global),
            };
            if json {
                let map: serde_json::Map<String, Value> = env
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for (k, v) in env {
                    println!("export {k}={}", shell_quote(&v));
                }
            }
            Ok(())
        }
        Cmd::Worktree(WorktreeCmd::New { name, base }) => {
            let r = create(root_of(&cwd)?, name, base).await?;
            eprintln!("{}", r.url);
            println!("{}", r.path.display());
            Ok(())
        }
        Cmd::Worktree(WorktreeCmd::Rm { name, force }) => remove(root_of(&cwd)?, name, force).await,
        Cmd::Worktree(WorktreeCmd::List) => {
            let root = root_of(&cwd)?;
            let s: daemon::Status = client::get("/status").await?;
            let p = s
                .projects
                .iter()
                .find(|p| p.project.root == root)
                .context("this project is not registered; run `localforest serve`")?;
            for c in p.checkouts.iter().filter(|c| c.checkout.worktree.is_some()) {
                println!(
                    "{}\t{}\t{}",
                    c.checkout.worktree.as_deref().unwrap_or_default(),
                    c.url,
                    c.checkout.path.display()
                );
            }
            Ok(())
        }
        Cmd::Status => {
            print_status(&client::get("/status").await?);
            Ok(())
        }
        Cmd::Sync { path } => {
            client::post::<Value>(
                "/sync",
                &daemon::RootReq {
                    root: root_of(&path)?,
                },
            )
            .await?;
            Ok(())
        }
        Cmd::Snapshot { path } => {
            client::post::<Value>(
                "/db/snapshot",
                &daemon::RootReq {
                    root: root_of(&path)?,
                },
            )
            .await?;
            Ok(())
        }
        Cmd::Trust => {
            let ca = tls::Ca::load_or_create(&config::home().join("ca"))?;
            tls::trust(&ca)?;
            eprintln!("trusted {}", config::ca_cert_path().display());
            eprintln!(
                "Firefox and Node use their own stores: NODE_EXTRA_CA_CERTS is in `localforest env`."
            );
            Ok(())
        }
        Cmd::Down { eject } => {
            let _ = client::post::<Value>("/shutdown", &()).await;
            if eject {
                for _ in 0..100 {
                    if client::get::<Value>("/status").await.is_err() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                ramdisk::eject(&config::pg_dir())?;
            }
            Ok(())
        }
        Cmd::Service(cmd) => {
            let (root, here, _) = config::locate(&cwd)?;
            let req = |a: &ServiceArgs| daemon::ServiceReq {
                root: root.clone(),
                worktree: match a.worktree.as_deref() {
                    Some(".") => None,
                    Some(w) => Some(w.to_string()),
                    None => here.clone(),
                },
                service: a.service.clone(),
            };
            match cmd {
                ServiceCmd::Start(a) => {
                    let r: Value = client::post("/service/start", &req(&a)).await?;
                    if let Some(url) = r["url"].as_str() {
                        println!("{url}");
                    }
                    eprintln!("log: {}", r["log"].as_str().unwrap_or_default());
                }
                ServiceCmd::Stop(a) => {
                    client::post::<Value>("/service/stop", &req(&a)).await?;
                }
                ServiceCmd::Restart(a) => {
                    client::post::<Value>("/service/stop", &req(&a)).await?;
                    let r: Value = client::post("/service/start", &req(&a)).await?;
                    if let Some(url) = r["url"].as_str() {
                        println!("{url}");
                    }
                }
                ServiceCmd::Log(a) => {
                    let r: Value = client::post("/service/log", &req(&a)).await?;
                    println!("{}", r["log"].as_str().unwrap_or_default());
                }
            }
            Ok(())
        }
        Cmd::Lsp { command } => {
            // The language servers are gone with the router (kill_on_drop); exit now:
            // the runtime would wait forever on its blocking stdin reader.
            let code = match lsp::run(command).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    1
                }
            };
            std::process::exit(code)
        }
        Cmd::Hook(h) => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let v: Value = serde_json::from_str(&input).context("hook input is not JSON")?;
            let dir = v
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from))
                .unwrap_or(cwd);
            match h {
                HookCmd::WorktreeCreate => {
                    let name = v["name"].as_str().context("no name in hook input")?;
                    let r = create(root_of(&dir)?, config::dns_label(name), None).await?;
                    println!("{}", r.path.display());
                }
                HookCmd::WorktreeRemove => {
                    let path = v["worktree_path"].as_str().context("no worktree_path")?;
                    if !Path::new(path).join(".git").exists() {
                        return Ok(());
                    }
                    let root = root_of(Path::new(path))?;
                    remove(root, path.to_string(), true).await?;
                }
            }
            Ok(())
        }
    }
}
