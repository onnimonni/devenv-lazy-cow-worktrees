//! `lazy-cow-tree shell-hook`: the checkout's environment for the calling shell (and
//! what it overrode, to restore when it leaves), and the worktree process marker the
//! shell keeps open.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    config::{self, Global, Project, ProjectSettings},
    daemon, server, worktree,
};

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What `shell-hook` set in the calling shell, kept there as JSON in `SHELL_STATE`.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
pub(crate) struct ShellState {
    /// Variables it set.
    pub(crate) set: Vec<String>,
    /// Their values from before it first set them (none: they were unset).
    pub(crate) was: BTreeMap<String, String>,
}

pub(crate) const SHELL_STATE: &str = "LAZY_COW_TREE_SHELL";

impl ShellState {
    pub(crate) fn from_env() -> Self {
        std::env::var(SHELL_STATE)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// `name`'s value before any hook set it.
    pub(crate) fn original(&self, name: &str) -> Option<String> {
        if self.set.iter().any(|n| n == name) {
            self.was.get(name).cloned()
        } else {
            std::env::var(name).ok()
        }
    }

    /// Shell code taking the shell from this state to `env`; `current` reads its variables.
    pub(crate) fn script(
        &self,
        env: &[(String, String)],
        current: impl Fn(&str) -> Option<String>,
    ) -> String {
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
pub(crate) fn marker_script(
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
pub(crate) fn checkout_env(
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
pub(crate) fn hook_env(
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
pub(crate) fn worktree_devenv_env(
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
