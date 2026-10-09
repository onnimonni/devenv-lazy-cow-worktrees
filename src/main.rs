//! lazy-cow-tree: local dev infrastructure for many git worktrees at once, made for coding
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
//! See README.md, `lazy-cow-tree --help` and devenv-module/devenv.nix.

mod client;
mod config;
mod cow;
mod daemon;
mod devenv_proxy;
mod github;
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
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
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
    /// Stand-in for devenv's `devenv-proxy` (DEVENV_PROXY_BINARY): run the daemon on
    /// the ports and control socket devenv asks for. Also runs as
    /// `lazy-cow-tree-devenv-proxy [ARGS]`.
    DevenvProxy {
        #[arg(long)]
        listen: std::net::SocketAddr,
        #[arg(long)]
        https_listen: Option<std::net::SocketAddr>,
        #[arg(long)]
        control_socket: PathBuf,
    },
    /// For the devenv module's shell hook: prints what takes the calling shell to the
    /// environment of this directory's checkout (PORT, DATABASE_URL, REDIS_URL, ...),
    /// restoring or unsetting what an earlier call set and this one doesn't (outside
    /// the project's checkouts: everything).
    #[command(hide = true)]
    ShellHook {
        #[command(flatten)]
        project: ProjectSettings,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// This service's environment [default: the default service's].
        #[arg(short, long)]
        service: Option<String>,
    },
    /// For the devenv module's `git` wrapper: provision new worktrees of this
    /// directory's project and clean up removed ones now.
    #[command(hide = true)]
    Reconcile {
        #[arg(long, default_value = ".")]
        path: PathBuf,
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
    /// The Let's Encrypt certificate of the project's domain (`lazyCowTree.tls`).
    #[command(subcommand)]
    Cert(CertCmd),
    /// Stop the daemon; --eject also drops the RAM disk with every database.
    Down {
        #[arg(long)]
        eject: bool,
    },
    /// A checkout's services (`lazy-cow-tree.services`).
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Worktree-aware LSP proxy: lazy-cow-tree lsp -- <server> [args]
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
    /// What still runs in a worktree: started there (by a shell in it, even if it
    /// detached since) and not run by lazy-cow-tree (the daemon's services, which it
    /// stops itself; `lazy-cow-tree lsp`'s language servers).
    /// For the `git` wrapper's `worktree remove`.
    Procs {
        path: PathBuf,
        /// SIGKILL them (with their descendants) first; lists what survived.
        #[arg(long)]
        kill: bool,
    },
}

#[derive(Subcommand)]
enum CertCmd {
    /// The certificates of the project's domain, the GitHub repository's newest
    /// https-certificate artifact (only read while the repository is private):
    /// their names and expiry, the names the project needs, and the
    /// HTTPS_CERTIFICATE_DOMAINS value for the repository's certificate workflow.
    Show {
        #[command(flatten)]
        project: ProjectSettings,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
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
    /// Extra environment of its process in this checkout, kept out of git; applied on
    /// its next start. Without flags: the names set.
    Env {
        #[command(flatten)]
        svc: ServiceArgs,
        /// KEY=VALUE to set (repeatable).
        #[arg(long, value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// KEY to remove (repeatable).
        #[arg(long, value_name = "KEY")]
        unset: Vec<String>,
    },
}

#[derive(clap::Args)]
struct ServiceArgs {
    /// Service [default: the default service].
    #[arg(short, long)]
    service: Option<String>,
    /// Worktree [default and `.`: the checkout you're in].
    #[arg(conflicts_with = "primary")]
    worktree: Option<String>,
    /// The primary checkout.
    #[arg(long)]
    primary: bool,
}

impl ServiceArgs {
    /// The worktree it names (None: the primary), `here` the checkout you're in.
    fn worktree(&self, here: Option<String>) -> Option<String> {
        match self.worktree.as_deref() {
            _ if self.primary => None,
            None | Some(".") => here,
            Some(w) => Some(w.to_string()),
        }
    }
}

#[derive(Subcommand)]
enum HookCmd {
    /// WorktreeCreate: {"name": ...} -> prints the path.
    WorktreeCreate,
    /// WorktreeRemove: {"worktree_path": ...}
    WorktreeRemove,
    /// PreToolUse of Edit/Write (lazyCowTree.protectPrimary): refuses files of the
    /// primary checkout at <root>.
    GuardPrimary {
        root: PathBuf,
        /// Where the refusal suggests the worktree.
        #[arg(default_value = ".claude/worktrees")]
        worktrees_dir: String,
    },
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What `shell-hook` set in the calling shell, kept there as JSON in `SHELL_STATE`.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
struct ShellState {
    /// Variables it set.
    set: Vec<String>,
    /// Their values from before it first set them (none: they were unset).
    was: BTreeMap<String, String>,
}

const SHELL_STATE: &str = "LAZY_COW_TREE_SHELL";

impl ShellState {
    fn from_env() -> Self {
        std::env::var(SHELL_STATE)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// `name`'s value before any hook set it.
    fn original(&self, name: &str) -> Option<String> {
        if self.set.iter().any(|n| n == name) {
            self.was.get(name).cloned()
        } else {
            std::env::var(name).ok()
        }
    }

    /// Shell code taking the shell from this state to `env`; `current` reads its variables.
    fn script(&self, env: &[(String, String)], current: impl Fn(&str) -> Option<String>) -> String {
        let mut next = ShellState::default();
        for (k, _) in env {
            if next.set.contains(k) {
                continue;
            }
            let was = if self.set.contains(k) {
                self.was.get(k).cloned()
            } else {
                current(k)
            };
            if let Some(v) = was {
                next.was.insert(k.clone(), v);
            }
            next.set.push(k.clone());
        }
        let mut out = String::new();
        for k in self.set.iter().filter(|k| !next.set.contains(k)) {
            match self.was.get(k) {
                Some(v) => out.push_str(&format!("export {k}={}\n", shell_quote(v))),
                None => out.push_str(&format!("unset {k}\n")),
            }
        }
        for (k, v) in env {
            out.push_str(&format!("export {k}={}\n", shell_quote(v)));
        }
        if next.set.is_empty() {
            out.push_str(&format!("unset {SHELL_STATE}\n"));
        } else {
            let json = serde_json::to_string(&next).unwrap_or_default();
            out.push_str(&format!("export {SHELL_STATE}={}\n", shell_quote(&json)));
        }
        out
    }
}

/// Shell code keeping the shell's process marker (`worktree::MARKER`, `<fd>:<path>`)
/// open on `want` (`worktree::marker_path` of the worktree it is in), closing one of
/// another worktree: what the shell starts inherits it, so `worktree procs` finds it
/// even after it detached.
/// `open_on(fd, path)`: whether the shell's `fd` is still that marker (the hook runs
/// with the shell's fds; one whose environment outlived its fds mustn't close another).
/// bash (3.2 too) takes fd 213; zsh only names fds above 9 through a variable.
fn marker_script(
    want: Option<&Path>,
    current: Option<&str>,
    open_on: impl Fn(i32, &str) -> bool,
) -> String {
    const M: &str = worktree::MARKER;
    let current = current.and_then(|c| {
        let (fd, path) = c.split_once(':')?;
        Some((fd.parse::<i32>().ok()?, path))
    });
    let want = want.map(|p| p.to_string_lossy().into_owned());
    if let (Some((fd, path)), Some(w)) = (current, &want)
        && path == w
        && open_on(fd, path)
    {
        return String::new();
    }
    let mut out = String::new();
    if let Some((fd, path)) = current
        && open_on(fd, path)
    {
        out.push_str(&format!(
            "if [ -n \"${{ZSH_VERSION:-}}\" ]; then __lct_fd={fd}; exec {{__lct_fd}}<&-; else exec {fd}<&-; fi\n"
        ));
    }
    match &want {
        Some(w) => {
            let q = shell_quote(w);
            // No `{ exec ...; } 2>/dev/null`: bash 3.2 undoes the exec when the group
            // ends. And it keeps an fd open already (inherited) on `exec 213<`: closed first.
            out.push_str(&format!(
                "unset {M}\n\
                 if [ -n \"${{ZSH_VERSION:-}}\" ]; then [ -r {q} ] && exec {{__lct_fd}}<{q} && export {M}=\"$__lct_fd:\"{q}; \
                 else exec 213<&-; [ -r {q} ] && exec 213<{q} && export {M}={}; fi\n",
                shell_quote(&format!("213:{w}"))
            ));
        }
        None if current.is_some() => out.push_str(&format!("unset {M}\n")),
        None => {}
    }
    if !out.is_empty() {
        out.push_str("unset __lct_fd\n");
    }
    out
}

/// The environment of `path`'s checkout (`service`'s, else the default service's).
fn checkout_env(
    g: &Global,
    p: Project,
    wt: Option<&str>,
    co_path: PathBuf,
    service: Option<&str>,
) -> Result<Vec<(String, String)>> {
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
        Some(port) => p.checkout_on(wt, co_path, port),
        None => p.checkout(wt, co_path),
    };
    Ok(match service {
        Some(s) if c.service(s).is_none() => anyhow::bail!("no service {s}"),
        Some(s) => c.service_env(g, Some(s)),
        None => c.env(g),
    })
}

/// `shell-hook`: the environment of `path`'s checkout, if it's in this devenv's project
/// (`DEVENV_ROOT`; `settings` are its) or another one the daemon registered (with its
/// settings); else none.
fn hook_env(
    g: &Global,
    mut settings: ProjectSettings,
    path: &Path,
    service: Option<&str>,
    state: &ShellState,
) -> Vec<(String, String)> {
    let Ok((root, wt, co_path)) = config::locate(path) else {
        return Vec::new();
    };
    let ours = state
        .original("DEVENV_ROOT")
        .and_then(|d| config::primary_root(Path::new(&d)).ok())
        .is_some_and(|r| r == root);
    let p = if ours {
        // The setting, not the name an earlier hook exported (maybe another project's).
        settings.name = state.original("LAZY_COW_TREE_PROJECT");
        let mut p = Project::new(root.clone(), settings);
        p.env = std::env::vars().collect();
        p
    } else {
        match daemon::registered_projects()
            .into_iter()
            .find(|p| p.root == root)
        {
            Some(p) => p,
            None => return Vec::new(),
        }
    };
    let mut env = checkout_env(g, p, wt.as_deref(), co_path.clone(), service).unwrap_or_default();
    if ours && wt.is_some() {
        env.extend(worktree_devenv_env(&root, &co_path, state, &env));
    }
    env
}

/// In a worktree of this devenv's project: its own DEVENV_ROOT, _DOTFILE, _STATE and
/// _RUNTIME, and every other variable of the (primary's) devenv shell naming a path in
/// the primary checkout moved to the worktree (UV_CONSTRAINT, ...), as services run
/// there get them (`server::command`); `set` (the checkout's own) wins.
fn worktree_devenv_env(
    root: &Path,
    worktree: &Path,
    state: &ShellState,
    set: &[(String, String)],
) -> Vec<(String, String)> {
    let skip = |k: &str| {
        matches!(k, "PWD" | "OLDPWD" | "_" | SHELL_STATE) || set.iter().any(|(s, _)| s == k)
    };
    let devenv = server::devenv_vars(worktree);
    let mut out: Vec<(String, String)> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| !skip(k) && !devenv.iter().any(|(d, _)| d == k))
        .filter_map(|k| {
            let original = state.original(&k)?;
            let moved = config::rewrite_root(&original, root, worktree);
            (moved != original).then_some((k, moved))
        })
        .collect();
    out.sort();
    out.extend(devenv);
    out
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

/// A worktree name as is; a path (`.`, `../x`, `/abs`) made absolute here: the daemon
/// has another cwd.
fn rm_target(cwd: &Path, name: &str) -> Result<String> {
    if config::valid_label(name) {
        return Ok(name.to_string());
    }
    match cwd.join(name).canonicalize() {
        Ok(path) => Ok(path.to_string_lossy().into_owned()),
        // A branch: `feat/login`.
        Err(_) if worktree::name_of_branch(name).is_ok() => Ok(name.to_string()),
        Err(_) => anyhow::bail!("no worktree {name}"),
    }
}

async fn remove(root: PathBuf, name: String, force: bool) -> Result<()> {
    // The daemon spares these and their ancestors: the shell, the Claude Code session.
    let keep = worktree::ancestors();
    let r: Value = client::post(
        "/worktrees/remove",
        &daemon::RemoveReq {
            root,
            name,
            force,
            keep_pids: keep,
        },
    )
    .await?;
    for w in r["warnings"].as_array().into_iter().flatten() {
        eprintln!("warning: {}", w.as_str().unwrap_or_default());
    }
    Ok(())
}

/// A command line on one line, at most 120 characters.
fn short_command(c: &str) -> String {
    let c = c.replace(['\n', '\r'], " ");
    match c.char_indices().nth(119) {
        Some((i, _)) => format!("{}…", &c[..i]),
        None => c,
    }
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
            if let Some(e) = &c.migrate_error {
                // Its first lines; the rest is the log's tail.
                for l in e.lines().take(4) {
                    println!("    {l}");
                }
            }
            for s in &c.services {
                println!(
                    "    {:<20} :{:<5} {:<45} {}",
                    s.name,
                    s.port,
                    s.url.as_deref().unwrap_or("(no http)"),
                    if s.running {
                        "running"
                    } else if s.stopped {
                        "stopped (until `service start`)"
                    } else {
                        "-"
                    }
                );
            }
        }
    }
}

/// Invoked as `lazy-cow-tree-devenv-proxy` (a link for DEVENV_PROXY_BINARY, which
/// takes one path): the `devenv-proxy` subcommand.
fn devenv_proxy_args(args: impl Iterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = args.collect();
    let invoked = args
        .first()
        .and_then(|a| std::path::Path::new(a).file_name())
        .is_some_and(|n| n == "lazy-cow-tree-devenv-proxy");
    if invoked {
        args.insert(1, "devenv-proxy".into());
    }
    args
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lazy_cow_tree=info".into()),
        )
        .init();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cli = Cli::parse_from(devenv_proxy_args(std::env::args_os()));
    let cwd = std::env::current_dir()?;

    match cli.command {
        Cmd::Serve { project, path } => {
            worktree::drop_marker();
            let p = project_for(&path, project)?;
            daemon::serve(cli.global, Some(p)).await
        }
        Cmd::DevenvProxy {
            listen,
            https_listen,
            control_socket,
        } => {
            worktree::drop_marker();
            let global = Global {
                http_port: listen.port(),
                https_port: https_listen.map_or(cli.global.https_port, |a| a.port()),
                devenv_proxy_socket: Some(control_socket),
                ..cli.global
            };
            daemon::serve(global, None).await
        }
        Cmd::ShellHook {
            project,
            path,
            service,
        } => {
            let state = ShellState::from_env();
            let env = hook_env(&cli.global, project, &path, service.as_deref(), &state);
            print!("{}", state.script(&env, |k| std::env::var(k).ok()));
            let worktree = config::locate(&path)
                .ok()
                .and_then(|(_, wt, co_path)| wt.and_then(|_| worktree::marker_path(&co_path)));
            let current = std::env::var(worktree::MARKER).ok();
            print!(
                "{}",
                marker_script(
                    worktree.as_deref(),
                    current.as_deref(),
                    worktree::fd_open_on
                )
            );
            Ok(())
        }
        Cmd::Reconcile { path } => {
            let root = root_of(&path)?;
            client::post::<Value>("/reconcile", &daemon::RootReq { root }).await?;
            Ok(())
        }
        Cmd::Worktree(WorktreeCmd::New { name, base }) => {
            let r = create(root_of(&cwd)?, name, base).await?;
            eprintln!("{}", r.url);
            println!("{}", r.path.display());
            Ok(())
        }
        Cmd::Worktree(WorktreeCmd::Rm { name, force }) => {
            remove(root_of(&cwd)?, rm_target(&cwd, &name)?, force).await
        }
        Cmd::Worktree(WorktreeCmd::List) => {
            let root = root_of(&cwd)?;
            let s: daemon::Status = client::get("/status").await?;
            let p = s
                .projects
                .iter()
                .find(|p| p.project.root == root)
                .context("this project is not registered; run `lazy-cow-tree serve`")?;
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
        Cmd::Worktree(WorktreeCmd::Procs { path, kill }) => {
            let dir = path
                .canonicalize()
                .with_context(|| format!("no worktree {}", path.display()))?;
            let keep = worktree::ancestors();
            if kill {
                worktree::kill_processes_in(&dir, &keep, true).await;
            }
            let mut out = std::io::stdout().lock();
            for l in worktree::processes_left(&dir, &keep) {
                // A closed pipe (`| head`) is no error.
                if writeln!(
                    out,
                    "pid {} with command \"{}\" was launched from this worktree and is still running",
                    l.pid,
                    short_command(&l.command)
                )
                .is_err()
                {
                    break;
                }
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
            if !tls::trust(&ca)? {
                eprintln!("already trusted {}", config::ca_cert_path().display());
                return Ok(());
            }
            eprintln!("trusted {}", config::ca_cert_path().display());
            eprintln!(
                "Firefox and Node use their own stores: NODE_EXTRA_CA_CERTS is in the devenv shell."
            );
            Ok(())
        }
        Cmd::Cert(CertCmd::Show { project, path }) => {
            let p = project_for(&path, project)?;
            let wanted = p.tls_names();
            if wanted.is_empty() {
                anyhow::bail!("no lazyCowTree.tls.domain (LAZY_COW_TREE_TLS_DOMAIN)");
            }
            let gh = tls::trusted::repo_client(&p)?;
            let now = tls::trusted::now();
            let mut have = Vec::new();
            match tls::trusted::fetch(&gh).await? {
                Some(zip) => {
                    for c in tls::trusted::certificates(&zip)? {
                        let expires = time::OffsetDateTime::from_unix_timestamp(c.leaf.not_after)?;
                        let state = if c.leaf.valid_at(now) {
                            "expires"
                        } else {
                            "EXPIRED"
                        };
                        println!(
                            "{}: {} names, {state} {expires}",
                            c.file,
                            c.leaf.names.len()
                        );
                        if c.leaf.valid_at(now) {
                            have.push(c.leaf);
                        }
                    }
                }
                None => println!("no {} artifact in {} yet", tls::trusted::ARTIFACT, gh.repo),
            }
            let missing: Vec<_> = wanted
                .iter()
                .filter(|n| !have.iter().any(|leaf| leaf.covers(n)))
                .collect();
            if !missing.is_empty() {
                println!("missing:");
                for n in missing {
                    println!("  {n}");
                }
            }
            println!(
                "\ngh variable set {} --repo {} --body '{}'",
                tls::trusted::DOMAINS_VARIABLE,
                gh.repo,
                tls::trusted::brace_names(&wanted).join(",")
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
                // A durable cluster (LAZY_COW_TREE_POSTGRES_DURABLE) has no RAM disk.
                if config::pg_dir().exists() {
                    ramdisk::eject(&config::pg_dir())?;
                }
            }
            Ok(())
        }
        Cmd::Service(cmd) => {
            let (root, here, _) = config::locate(&cwd)?;
            let req = |a: &ServiceArgs| daemon::ServiceReq {
                root: root.clone(),
                worktree: a.worktree(here.clone()),
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
                ServiceCmd::Env { svc, set, unset } => {
                    let r: Value = client::post(
                        "/service/env",
                        &daemon::ServiceEnvReq {
                            service: req(&svc),
                            set,
                            unset,
                        },
                    )
                    .await?;
                    // Names only: values may be secrets.
                    for k in r["keys"].as_array().into_iter().flatten() {
                        println!("{}", k.as_str().unwrap_or_default());
                    }
                    if r["changed"].as_bool() == Some(true) && r["running"].as_bool() == Some(true)
                    {
                        eprintln!(
                            "applied on its next start: `lazy-cow-tree service restart{}`",
                            svc.service
                                .as_deref()
                                .map(|s| format!(" -s {s}"))
                                .unwrap_or_default()
                        );
                    }
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
                    // `feat/login` stays its branch; anything else becomes a label.
                    let name = match worktree::name_of_branch(name) {
                        Ok(_) => name.to_string(),
                        Err(_) => config::worktree_label(name),
                    };
                    let r = create(root_of(&dir)?, name, None).await?;
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
                HookCmd::GuardPrimary {
                    root,
                    worktrees_dir,
                } => {
                    let input = &v["tool_input"];
                    let file = input["file_path"]
                        .as_str()
                        .or_else(|| input["notebook_path"].as_str());
                    if let Some(file) = file
                        && config::in_primary(&dir.join(file), &root)
                    {
                        // Exit 2: Claude Code refuses the tool call and shows this to the model.
                        eprintln!(
                            "{} is the primary checkout, kept for `git pull` only (lazyCowTree.protectPrimary): don't edit {file} there. Work in a worktree: `git worktree add {worktrees_dir}/<name>`, then edit the file under it.",
                            root.display()
                        );
                        std::process::exit(2);
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_target_makes_paths_absolute() {
        let d = tempfile::TempDir::new().unwrap();
        let wt = d.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let abs = wt.canonicalize().unwrap().to_string_lossy().into_owned();
        assert_eq!(rm_target(d.path(), "feat-x").unwrap(), "feat-x");
        assert_eq!(rm_target(&wt, ".").unwrap(), abs);
        assert_eq!(rm_target(d.path(), "./wt").unwrap(), abs);
        assert_eq!(rm_target(Path::new("/"), &abs).unwrap(), abs);
        assert!(rm_target(d.path(), "./missing").is_err());
        assert_eq!(rm_target(d.path(), "feat/x").unwrap(), "feat/x");
    }

    #[test]
    fn service_worktree_dot_is_the_checkout_you_are_in() {
        let wt = |args: &[&str], here: Option<&str>| {
            let cli = Cli::try_parse_from(
                ["lazy-cow-tree", "service", "stop", "-s", "care"]
                    .iter()
                    .chain(args),
            )
            .unwrap();
            let Cmd::Service(ServiceCmd::Stop(a)) = cli.command else {
                panic!("not service stop");
            };
            a.worktree(here.map(str::to_string))
        };
        let here = Some("feat-x");
        assert_eq!(wt(&[], here).as_deref(), here);
        assert_eq!(wt(&["."], here).as_deref(), here);
        assert_eq!(wt(&["other"], here).as_deref(), Some("other"));
        assert_eq!(wt(&["--primary"], here), None);
        // In the primary checkout.
        assert_eq!(wt(&["."], None), None);
        assert!(
            Cli::try_parse_from(["lazy-cow-tree", "service", "stop", "x", "--primary"]).is_err()
        );
    }

    #[test]
    fn parses_devenv_proxy_arguments_under_its_link_name() {
        let args = [
            "/nix/store/x/bin/lazy-cow-tree-devenv-proxy",
            "--listen",
            "127.0.0.1:80",
            "--control-socket",
            "/tmp/devenv-proxy-me.sock",
            "--https-listen",
            "127.0.0.1:443",
        ]
        .map(std::ffi::OsString::from);
        let cli = Cli::try_parse_from(devenv_proxy_args(args.into_iter())).unwrap();
        let Cmd::DevenvProxy {
            listen,
            https_listen,
            control_socket,
        } = cli.command
        else {
            panic!("not devenv-proxy");
        };
        assert_eq!(listen.port(), 80);
        assert_eq!(https_listen.map(|a| a.port()), Some(443));
        assert_eq!(control_socket, Path::new("/tmp/devenv-proxy-me.sock"));
        // Under its own name the arguments are left alone.
        let plain = devenv_proxy_args(["lazy-cow-tree", "status"].map(Into::into).into_iter());
        assert_eq!(plain, ["lazy-cow-tree", "status"]);
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn state_of(script: &str) -> ShellState {
        let line = script
            .lines()
            .find_map(|l| l.strip_prefix(&format!("export {SHELL_STATE}='")))
            .unwrap();
        serde_json::from_str(line.strip_suffix('\'').unwrap()).unwrap()
    }

    #[test]
    fn marker_follows_the_worktree_the_shell_is_in() {
        let open = |_: i32, _: &str| true;
        let wt = Path::new("/src/app/.claude/worktrees/x");
        // Outside worktrees, none before: nothing to do.
        assert_eq!(marker_script(None, None, open), "");
        // Entering one opens it.
        let s = marker_script(Some(wt), None, open);
        assert!(s.contains("exec 213<'/src/app/.claude/worktrees/x'"));
        assert!(s.contains("export WORKTREE_PROCESS_MARKER='213:/src/app/.claude/worktrees/x'"));
        assert!(!s.contains("exec {__lct_fd}<&-"), "no zsh marker to close");
        // Still there: kept.
        let cur = "213:/src/app/.claude/worktrees/x";
        assert_eq!(marker_script(Some(wt), Some(cur), open), "");
        // Another worktree: closes the old one first.
        let s = marker_script(
            Some(Path::new("/src/app/.claude/worktrees/y")),
            Some(cur),
            open,
        );
        assert!(s.starts_with("if [ -n \"${ZSH_VERSION:-}\" ]; then __lct_fd=213; exec {__lct_fd}<&-; else exec 213<&-; fi\n"));
        assert!(s.contains("213:/src/app/.claude/worktrees/y"));
        // Left the worktrees: closed and unset.
        let s = marker_script(None, Some("12:/src/app/.claude/worktrees/x"), open);
        assert!(s.contains("__lct_fd=12; exec {__lct_fd}<&-; else exec 12<&-"));
        assert!(s.contains("unset WORKTREE_PROCESS_MARKER\n"));
        // An fd that isn't the marker any more (inherited environment, not the fd) is
        // left alone; the same worktree is opened again.
        let s = marker_script(Some(wt), Some(cur), |_, _| false);
        assert!(!s.contains("exec {__lct_fd}<&-"), "not the marker: kept");
        assert!(s.contains("exec 213<"));
    }

    #[test]
    fn short_commands_fit_one_line() {
        assert_eq!(short_command("npm exec\nvitest"), "npm exec vitest");
        let long = "x".repeat(200);
        assert_eq!(short_command(&long).chars().count(), 120);
    }

    #[test]
    fn shell_hook_restores_what_it_overrode() {
        // First checkout: PORT was the user's, DATABASE_URL unset.
        let user = |k: &str| (k == "PORT").then(|| "3000".to_string());
        let s1 =
            ShellState::default().script(&kv(&[("PORT", "4000"), ("DATABASE_URL", "a")]), user);
        assert!(s1.contains("export PORT='4000'\nexport DATABASE_URL='a'\n"));
        let st1 = state_of(&s1);
        assert_eq!(st1.set, ["PORT", "DATABASE_URL"]);
        assert_eq!(st1.was, BTreeMap::from([("PORT".into(), "3000".into())]));

        // Another checkout without DATABASE_URL: the original PORT is still remembered.
        let s2 = st1.script(&kv(&[("PORT", "20001")]), |_| Some("4000".into()));
        assert!(s2.starts_with("unset DATABASE_URL\nexport PORT='20001'\n"));
        let st2 = state_of(&s2);
        assert_eq!(st2.was["PORT"], "3000");

        // Outside the project: back to the user's.
        let s3 = st2.script(&[], |_| None);
        assert_eq!(s3, format!("export PORT='3000'\nunset {SHELL_STATE}\n"));
    }
}
