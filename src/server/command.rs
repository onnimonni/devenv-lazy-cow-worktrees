//! The command a service, migration or setup runs: argv split like a shell would, with
//! the project's environment, moved to the worktree it runs in.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result};
use sha2::Digest;

use crate::config::{self, Project};

/// `cmdline` split like a shell would, run directly (no shell) in `cwd` of the
/// checkout at `checkout` with the registering project's environment (its PATH finds
/// the program), then `env`.
/// In a worktree the project environment was captured in the primary checkout, so
/// its paths into the primary move to the worktree (`config::rewrite_root`: env values,
/// arguments, `cwd`, and a script whose text names the primary runs as a rewritten
/// copy), and DEVENV_ROOT / _DOTFILE / _STATE / _RUNTIME are the worktree's.
/// devenv's own postgres/redis state, exported into the captured project env even
/// though lazy-cow-tree serves them (the module keeps `services.*.enable` readable): a
/// process pointing at the primary's data directory would bypass lazy-cow-tree.
const DEVENV_SERVICE_STATE: &[&str] = &["PGDATA", "REDISDATA"];

/// The shell hook's variables in the captured project env. A service's bash would
/// source the hook (BASH_ENV) and take the shell's view of the checkout (PORT of the
/// default service); the user's own BASH_ENV / ZDOTDIR, kept aside by the hook, come back.
pub(super) fn without_shell_hook(env: Vec<(String, String)>) -> Vec<(String, String)> {
    let saved = |k: &str| {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let (bash_env, zdotdir) = (
        saved("LAZY_COW_TREE_BASH_ENV"),
        saved("LAZY_COW_TREE_ZDOTDIR"),
    );
    let hook = [
        "BASH_ENV",
        "ZDOTDIR",
        "LAZY_COW_TREE_BASH_ENV",
        "LAZY_COW_TREE_ZDOTDIR",
        "LAZY_COW_TREE_SHELL",
    ];
    env.into_iter()
        .filter(|(k, _)| !hook.contains(&k.as_str()))
        .chain(bash_env.map(|v| ("BASH_ENV".to_string(), v)))
        .chain(zdotdir.map(|v| ("ZDOTDIR".to_string(), v)))
        .collect()
}

pub fn command(
    project: &Project,
    checkout: &Path,
    cmdline: &str,
    cwd: &Path,
    env: Vec<(String, String)>,
) -> Result<tokio::process::Command> {
    let root = project.root.as_path();
    let worktree = checkout != root;
    let rw = |s: &str| {
        if worktree {
            config::rewrite_root(s, root, checkout)
        } else {
            s.to_string()
        }
    };
    let base: Vec<(String, String)> = without_shell_hook(
        project
            .env
            .iter()
            .filter(|(k, _)| !DEVENV_SERVICE_STATE.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), rw(v)))
            .collect(),
    );
    let argv = shell_words::split(cmdline)?;
    let (prog, args) = argv.split_first().context("empty command")?;
    let prog = rw(prog);
    let program = base
        .iter()
        .find(|(k, _)| k == "PATH")
        .filter(|_| !prog.contains('/'))
        .and_then(|(_, path)| {
            std::env::split_paths(path)
                .map(|d| d.join(&prog))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| PathBuf::from(&prog));
    let program = if worktree {
        rewritten_script(&program, root, checkout, &config::home().join("scripts"))
            .unwrap_or(program)
    } else {
        program
    };
    let mut c = tokio::process::Command::new(program);
    c.args(args.iter().map(|a| rw(a)))
        .current_dir(rw(&cwd.to_string_lossy()))
        .env_clear()
        .envs(base)
        .envs(env.into_iter().map(|(k, v)| {
            let v = rw(&v);
            (k, v)
        }))
        .stdin(Stdio::null());
    if worktree {
        c.envs(devenv_vars(checkout));
    }
    Ok(c)
}

/// DEVENV_* of a worktree: its own root, state and a short runtime dir (unix
/// sockets must fit 104 bytes).
pub fn devenv_vars(checkout: &Path) -> Vec<(String, String)> {
    let runtime = config::runtime_dir(checkout);
    let _ = std::fs::create_dir_all(&runtime);
    let dotfile = checkout.join(".devenv");
    vec![
        ("DEVENV_ROOT".into(), checkout.display().to_string()),
        ("DEVENV_DOTFILE".into(), dotfile.display().to_string()),
        (
            "DEVENV_STATE".into(),
            dotfile.join("state").display().to_string(),
        ),
        ("DEVENV_RUNTIME".into(), runtime.display().to_string()),
    ]
}

/// A script (devenv's `exec` compiled to a store file) whose text names the primary
/// checkout: a copy for `checkout` in `dir` (`<home>/scripts`), content-addressed.
pub(super) fn rewritten_script(
    program: &Path,
    root: &Path,
    checkout: &Path,
    dir: &Path,
) -> Option<PathBuf> {
    let meta = std::fs::metadata(program).ok()?;
    if !meta.is_file() || meta.len() > 1 << 20 {
        return None;
    }
    let text = String::from_utf8(std::fs::read(program).ok()?).ok()?;
    let new = config::rewrite_root(&text, root, checkout);
    if new == text {
        return None;
    }
    let h = hex::encode(&sha2::Sha256::digest(new.as_bytes())[..8]);
    let name = program.file_name()?.to_string_lossy();
    let path = dir.join(format!("{h}-{name}"));
    if !path.exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent()?).ok()?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, &new).ok()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    Some(path)
}
