//! Environment helpers for a checkout's processes: the variables that make its web
//! framework accept its hostname, and paths of the primary checkout moved to a
//! worktree.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use git2::Repository;

/// A checkout's extra environment for a service (`lazy-cow-tree service env`): in
/// its git admin dir, so out of git and gone with a worktree.
fn env_overrides_dir(checkout: &Path) -> Result<PathBuf> {
    Ok(Repository::open(checkout)?.path().join("lazy-cow-tree-env"))
}

fn env_overrides_path(checkout: &Path, service: &str) -> Result<PathBuf> {
    Ok(env_overrides_dir(checkout)?.join(format!("{service}.env")))
}

/// `KEY=VALUE` lines; a missing or unreadable file is none.
pub fn env_overrides(checkout: &Path, service: &str) -> Vec<(String, String)> {
    let Ok(text) =
        env_overrides_path(checkout, service).and_then(|p| Ok(std::fs::read_to_string(p)?))
    else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Set (`KEY=VALUE`) and remove variables of a checkout's service; the names now set.
pub fn edit_env_overrides(
    checkout: &Path,
    service: &str,
    set: &[String],
    unset: &[String],
) -> Result<Vec<String>> {
    let valid = |k: &str| {
        k.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let mut env: BTreeMap<String, String> = env_overrides(checkout, service).into_iter().collect();
    for kv in set {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("{kv}: not KEY=VALUE");
        };
        if !valid(k) {
            bail!("{k}: not a variable name");
        }
        if v.contains('\n') {
            bail!("{k}: a value of one line only");
        }
        env.insert(k.to_string(), v.to_string());
    }
    for k in unset {
        env.remove(k);
    }
    if !set.is_empty() || !unset.is_empty() {
        std::fs::create_dir_all(env_overrides_dir(checkout)?)?;
        let path = env_overrides_path(checkout, service)?;
        let text: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        // Values may be secrets.
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?
            .write_all(text.as_bytes())?;
    }
    Ok(env.into_keys().collect())
}

/// Variables that make the framework of the app in `dir` accept its
/// https://…localhost hostname, detected from its manifests:
/// Phoenix (`PHX_HOST`), Rails (`RAILS_DEVELOPMENT_HOSTS`, host authorization) and
/// Vite (`__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS`, allowed-hosts check).
pub fn framework_env(dir: &Path, host: &str) -> Vec<(String, String)> {
    let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let mut env = Vec::new();
    // Umbrella projects declare Phoenix in apps/*/mix.exs.
    let mut mix = read(dir.join("mix.exs"));
    if let Ok(apps) = std::fs::read_dir(dir.join("apps")) {
        for app in apps.flatten() {
            mix += &read(app.path().join("mix.exs"));
        }
    }
    if mix.contains("{:phoenix,") {
        env.push(("PHX_HOST".into(), host.into()));
    }
    let gemfile = read(dir.join("Gemfile"));
    if gemfile.contains("gem \"rails\"") || gemfile.contains("gem 'rails'") {
        env.push(("RAILS_DEVELOPMENT_HOSTS".into(), host.into()));
    }
    if read(dir.join("package.json")).contains("\"vite\"") {
        env.push(("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS".into(), host.into()));
    }
    env
}

/// `text` with each path starting with `from` (the primary checkout) moved to `to`
/// (a worktree): an occurrence counts at a path boundary (start, `:`, `=`, space,
/// quote) followed by the end, `/`, `:`, space or quote. One already under `to` (a
/// worktree inside the primary) is left alone.
pub fn rewrite_root(text: &str, from: &Path, to: &Path) -> String {
    let (from, to) = (from.to_string_lossy(), to.to_string_lossy());
    if from.is_empty() || from == to || !text.contains(from.as_ref()) {
        return text.to_string();
    }
    let rest_of_to = to.strip_prefix(from.as_ref());
    let starts = |c: char| matches!(c, ':' | '=' | ' ' | '"' | '\'' | '\n' | '\t');
    let ends = |c: char| matches!(c, '/' | ':' | ' ' | '"' | '\'' | '\n' | '\t' | ';');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = text[i..].find(from.as_ref()) {
        let at = i + pos;
        let after = at + from.len();
        let before_ok = text[..at].chars().next_back().is_none_or(starts);
        let tail = &text[after..];
        let after_ok = tail.chars().next().is_none_or(ends);
        let already = rest_of_to.is_some_and(|r| !r.is_empty() && tail.starts_with(r));
        out.push_str(&text[i..at]);
        if before_ok && after_ok && !already {
            out.push_str(&to);
        } else {
            out.push_str(&from);
        }
        i = after;
    }
    out.push_str(&text[i..]);
    out
}
