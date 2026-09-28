//! Worktree lifecycle, following git-cow: `git worktree add --no-checkout` (here via
//! libgit2), then fill it with copy-on-write clones of the primary checkout, build
//! caches included, so a new worktree costs ~0 disk and needs no rebuild.
//! Removal refuses to lose work unless forced.

use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use git2::{BranchType, Oid, Repository, Status, StatusOptions, WorktreePruneOptions};
use tracing::{info, warn};

use crate::{
    config::{Project, dns_label, valid_label},
    sync::Syncer,
};

#[derive(Debug, Clone)]
pub struct Info {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Linked worktrees of the repository at `root` that exist on disk.
pub fn list(root: &Path) -> Result<Vec<Info>> {
    let repo = Repository::open(root)?;
    let mut out: Vec<Info> = Vec::new();
    for admin in repo.worktrees()?.iter().filter_map(|n| n.ok().flatten()) {
        let Ok(wt) = repo.find_worktree(admin) else {
            continue;
        };
        if wt.validate().is_err() {
            continue;
        }
        let Ok(path) = wt.path().canonicalize() else {
            continue;
        };
        let name = dns_label(&path.file_name().unwrap_or_default().to_string_lossy());
        if out.iter().any(|i| i.name == name) {
            warn!(
                "worktree {} has the same name as another one ({name}); skipping it",
                path.display()
            );
            continue;
        }
        let branch = Repository::open_from_worktree(&wt)
            .and_then(|r| {
                let h = r.head()?;
                Ok(h.is_branch()
                    .then(|| h.shorthand().map(str::to_string).ok())
                    .flatten())
            })
            .ok()
            .flatten();
        out.push(Info { name, path, branch });
    }
    Ok(out)
}

/// `git worktree add --no-checkout` for an existing branch.
fn add_no_checkout(root: &Path, admin_name: &str, path: &Path, branch: &str) -> Result<()> {
    use libgit2_sys as raw;
    // Opening through git2 initialises libgit2.
    drop(Repository::open(root)?);
    let c_root = CString::new(root.as_os_str().as_bytes())?;
    let c_name = CString::new(admin_name)?;
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let c_ref = CString::new(format!("refs/heads/{branch}"))?;
    unsafe {
        let mut repo = std::ptr::null_mut();
        let rc = raw::git_repository_open(&mut repo, c_root.as_ptr());
        if rc < 0 {
            return Err(git2::Error::last_error(rc).into());
        }
        let mut reference = std::ptr::null_mut();
        let rc = raw::git_reference_lookup(&mut reference, repo, c_ref.as_ptr());
        if rc < 0 {
            let e = git2::Error::last_error(rc);
            raw::git_repository_free(repo);
            return Err(e.into());
        }
        let mut opts: raw::git_worktree_add_options = std::mem::zeroed();
        raw::git_worktree_add_options_init(&mut opts, raw::GIT_WORKTREE_ADD_OPTIONS_VERSION);
        opts.reference = reference;
        // Dry run: git-cow fills the worktree and its index.
        opts.checkout_options.checkout_strategy =
            raw::GIT_CHECKOUT_NONE | raw::GIT_CHECKOUT_DONT_UPDATE_INDEX;
        let mut wt = std::ptr::null_mut();
        let rc = raw::git_worktree_add(&mut wt, repo, c_name.as_ptr(), c_path.as_ptr(), &opts);
        let err = (rc < 0).then(|| git2::Error::last_error(rc));
        if !wt.is_null() {
            raw::git_worktree_free(wt);
        }
        raw::git_reference_free(reference);
        raw::git_repository_free(repo);
        if let Some(e) = err {
            return Err(e.into());
        }
    }
    Ok(())
}

/// Create worktree `name` on a new branch `name` from `base` (default: the freshly
/// fetched base branch, or the local one when it only adds commits on top).
pub fn create(
    project: &Project,
    syncer: &Syncer,
    name: &str,
    base: Option<&str>,
) -> Result<PathBuf> {
    if !valid_label(name) {
        bail!(
            "{name}: use at most 32 of a-z, 0-9 and '-'; it becomes {name}.{}.localhost",
            project.name
        );
    }
    let root = &project.root;
    let dir = project.worktrees_dir();
    let path = dir.join(name);
    if path.join(".git").exists() {
        let existing = path.canonicalize()?;
        if list(root)?.iter().any(|i| i.path == existing) {
            info!("worktree {name} already exists");
            return Ok(existing);
        }
        bail!("{} exists and is not a worktree", path.display());
    }
    if path.exists() {
        bail!("{} already exists", path.display());
    }

    let repo = Repository::open(root)?;
    let branch_exists = repo.find_branch(name, BranchType::Local).is_ok();
    if branch_exists {
        if base.is_some() {
            bail!("branch {name} already exists; drop the base to check it out");
        }
        info!("checking out existing branch {name}");
    } else {
        let commit = match base {
            Some(b) => repo
                .revparse_single(b)
                .with_context(|| format!("unknown base {b}"))?
                .peel_to_commit()?,
            None => {
                if let Err(e) = syncer.fetch(&repo) {
                    warn!("fetch failed, using local refs: {e:#}");
                }
                let local = repo
                    .find_reference(&format!("refs/heads/{}", syncer.base))
                    .and_then(|r| r.peel_to_commit())
                    .ok();
                let remote = repo
                    .find_reference(&format!("refs/remotes/{}/{}", syncer.remote, syncer.base))
                    .and_then(|r| r.peel_to_commit())
                    .ok();
                match (local, remote) {
                    (Some(l), Some(r)) => {
                        if l.id() == r.id() || repo.graph_descendant_of(l.id(), r.id())? {
                            l
                        } else {
                            r
                        }
                    }
                    (Some(c), None) | (None, Some(c)) => c,
                    (None, None) => repo.head()?.peel_to_commit()?,
                }
            }
        };
        repo.branch(name, &commit, false)?;
    }

    // A stale admin dir (worktree deleted by hand) would make the add fail.
    if let Ok(wt) = repo.find_worktree(name)
        && wt.validate().is_err()
    {
        wt.prune(Some(WorktreePruneOptions::new().working_tree(true)))?;
    }
    std::fs::create_dir_all(&dir)?;
    add_no_checkout(root, name, &path, name)?;
    populate(root, &path, name)?;
    Ok(path.canonicalize()?)
}

/// Fill a fresh worktree (only `.git` in it) with copy-on-write clones of the primary
/// checkout, gitignored build caches included (git-cow); a regular checkout if that
/// fails. Per-checkout state that names the primary's paths is left behind.
fn populate(root: &Path, path: &Path, name: &str) -> Result<()> {
    let opts = git_cow::PopulateOptions {
        from: Some(root.to_path_buf()),
        include_ignored: true,
        settle: false,
    };
    match git_cow::populate(path, &opts) {
        Ok(r) if r.cloned > 0 => {
            for w in &r.warnings {
                warn!("git-cow: {w}");
            }
            info!(
                "worktree {name}: {} clone, {} files written, carried {}",
                r.filesystem,
                r.rewritten,
                git_cow::join_paths(&r.carried)
            );
        }
        // A regular checkout (no copy-on-write here): the caches are ours to bring.
        Ok(_) => copy_caches(root, path, name)?,
        Err(e) => {
            warn!("git-cow: {e:#}; falling back to a regular checkout");
            let wt = Repository::open(path)?;
            wt.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
            copy_caches(root, path, name)?;
        }
    }
    for dir in PER_CHECKOUT {
        let p = path.join(dir);
        if p.exists() {
            std::fs::remove_dir_all(&p)?;
        }
    }
    // Never again for this worktree (its git admin dir goes away with it).
    std::fs::write(populated_marker(path)?, "")?;
    Ok(())
}

/// Top-level gitignored directories of the primary checkout (build caches) that
/// `dest` lacks; with a `.worktreeinclude`, only the ones it names.
fn missing_caches(root: &Path, dest: &Path) -> Result<Vec<String>> {
    let primary = Repository::open(root)?;
    let include = worktree_include(root);
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(root)?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !entry.file_type().is_ok_and(|t| t.is_dir())
            || include
                .as_ref()
                .is_some_and(|inc| !inc.contains(name_str.as_ref()))
            || NOT_CACHES.contains(&name_str.as_ref())
            || dest.join(&name).exists()
            || !primary.status_should_ignore(Path::new(&name)).unwrap_or(false)
            // Nested worktrees (.claude/worktrees) and other repositories.
            || entry.path().join(".git").exists()
            || dest.starts_with(entry.path())
        {
            continue;
        }
        missing.push(name_str.into_owned());
    }
    Ok(missing)
}

/// Copy a directory tree: reflinks where the filesystem has them, else bytes; keeps
/// modes, mtimes (build tools compare them) and symlinks.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let meta = std::fs::symlink_metadata(&from)?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else if kind.is_file() {
            reflink_copy::reflink_or_copy(&from, &to)?;
            std::fs::set_permissions(&to, meta.permissions())?;
        } else {
            // Sockets, fifos: live state, never cache.
            continue;
        }
        if !kind.is_symlink() {
            filetime::set_file_mtime(&to, filetime::FileTime::from_last_modification_time(&meta))?;
        }
    }
    Ok(())
}

/// git-cow couldn't clone (no copy-on-write on this filesystem, e.g. ext4): copy the
/// build caches ourselves, so the worktree still needs no fresh install/compile.
fn copy_caches(root: &Path, path: &Path, name: &str) -> Result<()> {
    let missing = missing_caches(root, path)?;
    if missing.is_empty() {
        return Ok(());
    }
    let t = std::time::Instant::now();
    for dir in &missing {
        copy_tree(&root.join(dir), &path.join(dir))?;
    }
    info!(
        "worktree {name}: no copy-on-write clone here; copied {} in {:?}",
        missing.join(", "),
        t.elapsed()
    );
    Ok(())
}

fn populated_marker(path: &Path) -> Result<PathBuf> {
    Ok(Repository::open(path)?.path().join("localforest-populated"))
}

/// First path segments named in the primary's `.worktreeinclude` (git-cow carries
/// only those ignored paths when it exists).
fn worktree_include(root: &Path) -> Option<HashSet<String>> {
    let text = std::fs::read_to_string(root.join(".worktreeinclude")).ok()?;
    Some(
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
            .filter_map(|l| {
                l.trim_start_matches('/')
                    .split('/')
                    .next()
                    .map(str::to_string)
            })
            .collect(),
    )
}

const ENV_BEGIN: &str = "# >>> localforest: this worktree's environment (regenerated) >>>";
const ENV_END: &str = "# <<< localforest <<<";

/// A `.env` value. Single-quoted, which dotenvy, Node and Ruby dotenv, docker compose,
/// direnv and `set -a; . .env` read verbatim (no escapes, no `$` expansion; Bun
/// still expands `$VAR` in single quotes), unless it holds `'` or a line break,
/// which single quotes can't carry; then double-quoted with `\\`, `\"`, `\$` and `\n`
/// escaped, the escapes dotenvy, Ruby dotenv and docker compose share (a `\r` stays
/// raw: dotenvy rejects the escape), and each backtick as a single-quoted piece
/// (`"a"'`'"b"`: dotenvy has no `` \` `` escape), so a shell sourcing it runs nothing.
/// Loaders differ on that fallback: Node dotenv keeps its backslashes, Ruby dotenv
/// and docker compose don't join quoted pieces, sh reads `\n` as backslash-n.
fn dotenv_quote(v: &str) -> String {
    if !v.contains(['\'', '\n', '\r']) {
        return format!("'{v}'");
    }
    let mut out = String::from("\"");
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            // No escape for it that dotenvy takes: a single-quoted piece.
            '`' => out.push_str("\"'`'\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Write `env` into the worktree's `.env` (as a block at the top, so first-wins
/// loaders see it), making sure `.env` is gitignored (`.git/info/exclude` if not)
/// and never touching a tracked one. Keys of the rest of the file (e.g. a `.env`
/// cloned from the primary checkout) that the block sets are commented out, so no
/// loader picks up the primary's DATABASE_URL.
pub fn write_env(path: &Path, env: &[(String, String)]) -> Result<()> {
    let repo = Repository::open(path)?;
    let file = path.join(".env");
    if repo.index()?.get_path(Path::new(".env"), 0).is_some() {
        warn!(
            "{}: .env is tracked by git; not writing localforest's environment into it",
            path.display()
        );
        return Ok(());
    }
    if !repo.status_should_ignore(Path::new(".env"))? {
        let exclude = repo.commondir().join("info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap())?;
        let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("# localforest writes each worktree's .env\n.env\n");
        std::fs::write(&exclude, text)?;
        info!("added .env to {}", exclude.display());
    }
    let old = std::fs::read_to_string(&file).unwrap_or_default();
    // Drop our previous block.
    let mut rest = String::new();
    let mut inside = false;
    for line in old.lines() {
        if line == ENV_BEGIN {
            inside = true;
        } else if line == ENV_END {
            inside = false;
        } else if !inside {
            rest.push_str(line);
            rest.push('\n');
        }
    }
    let ours: HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    let mut out = format!("{ENV_BEGIN}\n");
    for (k, v) in env {
        if v.contains('$') {
            warn!(
                "{}: .env value of {k} contains '$', which some loaders (Bun) expand",
                path.display()
            );
        }
        out.push_str(&format!("{k}={}\n", dotenv_quote(v)));
    }
    out.push_str(ENV_END);
    out.push('\n');
    for line in rest.lines() {
        let key = line
            .trim_start()
            .trim_start_matches("export ")
            .split('=')
            .next()
            .unwrap_or_default()
            .trim();
        if !line.trim_start().starts_with('#') && line.contains('=') && ours.contains(key) {
            out.push_str(&format!("# overridden by localforest: {line}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    std::fs::write(&file, out)?;
    Ok(())
}

/// Ignored state that belongs to one checkout: language server indexes (absolute
/// paths of the checkout they were built in) and environments.
const PER_CHECKOUT: &[&str] = &[".dexter", ".elixir_ls", ".lexical", ".expert"];

/// Top-level gitignored entries never worth carrying (or checking for).
const NOT_CACHES: &[&str] = &[
    ".git",
    ".devenv",
    ".direnv",
    ".claude",
    ".venv",
    "venv",
    "tmp",
    "log",
    ".env",
    ".dexter",
    ".elixir_ls",
    ".lexical",
    ".expert",
];

/// `git worktree lock`ed: automatic removal leaves it alone.
pub fn is_locked(root: &Path, info: &Info) -> bool {
    let Ok(repo) = Repository::open(root) else {
        return false;
    };
    repo.worktrees()
        .ok()
        .into_iter()
        .flat_map(|names| {
            names
                .iter()
                .filter_map(|n| n.ok().flatten().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter_map(|n| repo.find_worktree(&n).ok())
        .find(|wt| wt.path().canonicalize().ok().as_deref() == Some(info.path.as_path()))
        .is_some_and(|wt| matches!(wt.is_locked(), Ok(git2::WorktreeLockStatus::Locked(_))))
}

/// `git worktree add` is still creating it (git locks it meanwhile).
pub fn initializing(root: &Path, info: &Info) -> bool {
    let Ok(repo) = Repository::open(root) else {
        return false;
    };
    repo.worktrees()
        .ok()
        .into_iter()
        .flat_map(|names| {
            names
                .iter()
                .filter_map(|n| n.ok().flatten().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter_map(|n| repo.find_worktree(&n).ok())
        .find(|wt| wt.path().canonicalize().ok().as_deref() == Some(info.path.as_path()))
        .is_some_and(|wt| {
            matches!(
                wt.is_locked(),
                Ok(git2::WorktreeLockStatus::Locked(Some(ref r))) if r.starts_with("initializing")
            )
        })
}

/// A worktree made by plain `git worktree add` (not localforest or git-cow) has no
/// build caches. While it's still exactly its fresh checkout (clean, nothing
/// untracked) and the primary has gitignored top-level entries it lacks (`deps/`,
/// `_build/`, `node_modules/`, …), redo it as a copy-on-write clone of the primary,
/// caches included. Ok(true) when it did.
pub fn carry_caches(root: &Path, info: &Info) -> Result<bool> {
    // Only just created: an old worktree someone works in is left alone.
    let age = std::fs::symlink_metadata(info.path.join(".git"))?
        .modified()?
        .elapsed()
        .unwrap_or_default();
    if age > Duration::from_secs(600) {
        return Ok(false);
    }
    if populated_marker(&info.path)?.exists() {
        return Ok(false);
    }
    let missing = missing_caches(root, &info.path)?;
    if missing.is_empty() {
        return Ok(false);
    }
    if is_dirty(&info.path)? {
        info!(
            "worktree {}: lacks {} but has changes; not touching it",
            info.name,
            missing.join(", ")
        );
        return Ok(false);
    }
    info!(
        "worktree {}: made without copy-on-write (lacks {}); cloning it from the primary",
        info.name,
        missing.join(", ")
    );
    for entry in std::fs::read_dir(&info.path)?.flatten() {
        if entry.file_name() == ".git" {
            continue;
        }
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    populate(root, &info.path, &info.name)?;
    Ok(true)
}

/// Uncommitted and untracked (not ignored) paths.
pub fn uncommitted(path: &Path) -> Result<Vec<String>> {
    let repo = Repository::open(path)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        .exclude_submodules(true);
    Ok(repo
        .statuses(Some(&mut opts))?
        .iter()
        .filter(|e| e.status() != Status::CURRENT)
        .filter_map(|e| e.path().ok().map(str::to_string))
        .collect())
}

/// `a, b, c` (the first 20, then `…`).
pub fn short_list(items: &[String]) -> String {
    let mut s = items
        .iter()
        .take(20)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > 20 {
        s.push_str(", …");
    }
    s
}

pub fn is_dirty(path: &Path) -> Result<bool> {
    let repo = Repository::open(path)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        .exclude_submodules(true);
    Ok(repo
        .statuses(Some(&mut opts))?
        .iter()
        .any(|e| e.status() != Status::CURRENT))
}

/// Rebuilt or regenerated on their own: never reported as lost.
const REBUILDABLE: &[&str] = &[
    "_build",
    "deps",
    "node_modules",
    "target",
    ".git",
    ".devenv",
    ".direnv",
    ".venv",
    "venv",
];

/// Gitignored paths in the worktree at `path` that removal would delete for good:
/// not build caches or per-checkout indexes, and not copies of the primary checkout
/// at `root` (same file contents; directories with the same files and sizes).
/// Sockets and fifos are skipped.
pub fn ignored_files(root: &Path, path: &Path) -> Result<Vec<String>> {
    let repo = Repository::open(path)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(false)
        .include_ignored(true)
        .recurse_ignored_dirs(false)
        .exclude_submodules(true);
    Ok(repo
        .statuses(Some(&mut opts))?
        .iter()
        .filter(|e| e.status().contains(Status::IGNORED))
        .filter_map(|e| e.path().ok().map(str::to_string))
        .filter(|p| {
            let top = p.split('/').next().unwrap_or_default();
            let rel = p.trim_end_matches('/');
            !REBUILDABLE.contains(&top)
                && !PER_CHECKOUT.contains(&top)
                && !is_copy(&path.join(rel), &root.join(rel)).unwrap_or(false)
        })
        .collect())
}

/// Ok(true): `a` is the same as `b` (or not a file worth reporting: a socket, fifo).
fn is_copy(a: &Path, b: &Path) -> Result<bool> {
    let ma = std::fs::symlink_metadata(a)?;
    let kind = ma.file_type();
    if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
        return Ok(true);
    }
    let Ok(mb) = std::fs::symlink_metadata(b) else {
        return Ok(false);
    };
    if kind.is_symlink() {
        return Ok(mb.file_type().is_symlink() && std::fs::read_link(a)? == std::fs::read_link(b)?);
    }
    if kind.is_dir() {
        return Ok(mb.is_dir() && tree_sizes(a)? == tree_sizes(b)?);
    }
    if !mb.is_file() || ma.len() != mb.len() {
        return Ok(false);
    }
    if ma.modified()? == mb.modified()? {
        // Cloned or copied with its mtime: git-cow, copy_tree.
        return Ok(true);
    }
    same_bytes(a, b)
}

/// Every regular file under `dir` (relative path, size), sorted.
fn tree_sizes(dir: &Path) -> Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)?.flatten() {
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                let rel = entry.path().strip_prefix(dir)?.to_path_buf();
                out.push((rel, meta.len()));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn same_bytes(a: &Path, b: &Path) -> Result<bool> {
    use std::io::Read;
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(fb.read(&mut bb)? == 0);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

pub enum Safety {
    /// Nothing can be lost.
    Safe,
    /// Commits beyond the base branch: safe only if a PR merged `branch` at `head`.
    NeedsMergedPr { branch: String, head: Oid },
}

/// Can the worktree go without losing work? Err explains why not.
pub fn safety(info: &Info, remote: &str, base: &str) -> Result<Safety> {
    if is_dirty(&info.path)? {
        bail!("{} has uncommitted or untracked changes", info.name);
    }
    let repo = Repository::open(&info.path)?;
    let head = repo.head()?.peel_to_commit()?.id();
    let bases: Vec<Oid> = [
        format!("refs/heads/{base}"),
        format!("refs/remotes/{remote}/{base}"),
    ]
    .iter()
    .filter_map(|r| repo.find_reference(r).ok()?.peel_to_commit().ok())
    .map(|c| c.id())
    .collect();
    if bases.is_empty() {
        bail!("neither {base} nor {remote}/{base} exists");
    }
    if ahead(&repo, head, &bases, remote, base)? == 0 {
        return Ok(Safety::Safe);
    }
    match &info.branch {
        Some(b) => Ok(Safety::NeedsMergedPr {
            branch: b.clone(),
            head,
        }),
        None => bail!(
            "{} is on a detached HEAD with commits beyond {base}",
            info.name
        ),
    }
}

/// Commits in `head` not in `hidden`, ignoring conflict-free merges of the base
/// branch that localforest made (they add nothing of the branch's own). One with that
/// message but a tree other than the clean merge of its parents (edited, conflicts
/// resolved by hand) counts.
pub fn ahead(
    repo: &Repository,
    head: Oid,
    hidden: &[Oid],
    remote: &str,
    base: &str,
) -> Result<usize> {
    let mut walk = repo.revwalk()?;
    walk.push(head)?;
    for h in hidden {
        walk.hide(*h)?;
    }
    let auto = format!("Merge remote-tracking branch '{remote}/{base}' into ");
    let mut n = 0;
    for id in walk {
        let c = repo.find_commit(id?)?;
        if c.message().is_ok_and(|m| m.starts_with(&auto)) && is_clean_merge(repo, &c) {
            continue;
        }
        n += 1;
    }
    Ok(n)
}

/// Does a two-parent merge add nothing of its own? Every path it changed relative to
/// its first parent must match the second parent, or else the conflict-free merge
/// libgit2 computes. Cheap (tree diff; the merge is computed only when needed), and
/// tolerant of merges `git merge` made differently (renames, criss-cross): a path
/// taken from either side as is never counts. A path matching neither side nor the
/// clean merge (an edit, a conflict resolved by hand) does. Anything unreadable
/// (a parent missing in a shallow clone) counts as work. Cached per commit.
fn is_clean_merge(repo: &Repository, c: &git2::Commit) -> bool {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<Oid, bool>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(&c.id()) {
        return *v;
    }
    let clean = merge_adds_nothing(repo, c).unwrap_or(false);
    cache.lock().unwrap().insert(c.id(), clean);
    clean
}

fn merge_adds_nothing(repo: &Repository, c: &git2::Commit) -> Result<bool> {
    if c.parent_count() != 2 {
        return Ok(false);
    }
    let (p1, p2) = (c.parent(0)?, c.parent(1)?);
    let (t1, t2, tm) = (p1.tree()?, p2.tree()?, c.tree()?);
    let diff = repo.diff_tree_to_tree(Some(&t1), Some(&tm), None)?;
    let blob = |t: &git2::Tree, p: &Path| t.get_path(p).ok().map(|e| e.id());
    let mut clean: Option<Option<git2::Tree>> = None;
    for delta in diff.deltas() {
        let Some(p) = delta.new_file().path().or(delta.old_file().path()) else {
            return Ok(false);
        };
        let ours = blob(&tm, p);
        if ours == blob(&t2, p) {
            continue;
        }
        let merged = clean.get_or_insert_with(|| {
            let mut index = repo.merge_commits(&p1, &p2, None).ok()?;
            if index.has_conflicts() {
                return None;
            }
            repo.find_tree(index.write_tree_to(repo).ok()?).ok()
        });
        match merged {
            Some(t) if blob(t, p) == ours => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// When the worktree was made (its `.git` file's mtime), as unix seconds.
pub fn created_at(info: &Info) -> Result<i64> {
    let t = std::fs::symlink_metadata(info.path.join(".git"))?.modified()?;
    Ok(t.duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64)
}

/// Clock skew allowed between this machine and GitHub.
const MERGE_GRACE_SECS: i64 = 300;

/// Was the worktree made clearly (by [`MERGE_GRACE_SECS`]) before a PR merged at
/// `merged_at`? Err when unknown or too close to call.
pub fn made_before(info: &Info, merged_at: i64) -> Result<()> {
    let made = created_at(info)?;
    if made + MERGE_GRACE_SECS > merged_at {
        bail!(
            "{} was made {}s before the PR merged (or after); not the PR's worktree",
            info.name,
            merged_at - made
        );
    }
    Ok(())
}

/// After a PR merged `branch` at `pr_head`: is everything in the worktree in it?
pub fn covered_by_pr(info: &Info, pr_head: Oid, remote: &str, base: &str) -> Result<bool> {
    let repo = Repository::open(&info.path)?;
    let head = repo.head()?.peel_to_commit()?.id();
    let mut hidden = vec![pr_head];
    for r in [
        format!("refs/heads/{base}"),
        format!("refs/remotes/{remote}/{base}"),
    ] {
        if let Ok(c) = repo.find_reference(&r).and_then(|r| r.peel_to_commit()) {
            hidden.push(c.id());
        }
    }
    if repo.find_commit(pr_head).is_err() {
        return Ok(false);
    }
    Ok(ahead(&repo, head, &hidden, remote, base)? == 0)
}

/// Own commits of the worktree's HEAD that are neither on the base branch (local or
/// remote) nor pushed to its branch on `remote`: deleting the branch would lose them.
pub fn unpushed(info: &Info, remote: &str, base: &str) -> Result<usize> {
    let repo = Repository::open(&info.path)?;
    let head = repo.head()?.peel_to_commit()?.id();
    let mut refs = vec![
        format!("refs/heads/{base}"),
        format!("refs/remotes/{remote}/{base}"),
    ];
    if let Some(b) = &info.branch {
        refs.push(format!("refs/remotes/{remote}/{b}"));
    }
    let hidden: Vec<Oid> = refs
        .iter()
        .filter_map(|r| repo.find_reference(r).ok()?.peel_to_commit().ok())
        .map(|c| c.id())
        .collect();
    ahead(&repo, head, &hidden, remote, base)
}

/// Move the worktree out of the way, drop git's metadata and its own branch (kept
/// as `<name>-kept-<sha>` without `delete_branch`: Ok(Some(that name))), then delete
/// the files in the background.
pub fn remove_files(root: &Path, info: &Info, delete_branch: bool) -> Result<Option<String>> {
    let repo = Repository::open(root)?;
    let trash_dir = repo.commondir().join("localforest-trash");
    std::fs::create_dir_all(&trash_dir)?;
    let trash = trash_dir.join(format!("{}.{}", info.name, std::process::id()));
    let mut trash = if trash.exists() {
        trash_dir.join(format!(
            "{}.{}.{}",
            info.name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ))
    } else {
        trash
    };
    // Ours (even if locked), then any other stale unlocked admin dir (what `git
    // worktree prune` does: worktrees deleted by hand). Locked ones, e.g. on an
    // unmounted volume, are left alone.
    let own = repo
        .worktrees()?
        .iter()
        .filter_map(|n| n.ok().flatten().map(str::to_string))
        .find(|n| {
            repo.find_worktree(n)
                .is_ok_and(|wt| wt.path().canonicalize().ok().as_deref() == Some(&*info.path))
        });
    // Same volume: a rename instead of deleting thousands of files in the foreground.
    // Worktrees on another volume than the git dir go to a hidden sibling instead.
    if let Err(e) = std::fs::rename(&info.path, &trash) {
        if e.raw_os_error() != Some(libc::EXDEV) {
            return Err(e).with_context(|| format!("moving {} away", info.path.display()));
        }
        trash = info.path.with_file_name(format!(
            "{SIBLING_TRASH}{}",
            trash.file_name().unwrap_or_default().to_string_lossy()
        ));
        std::fs::rename(&info.path, &trash)
            .with_context(|| format!("moving {} away", info.path.display()))?;
    }
    // The worktree is gone now: the rest is tidying up, not worth failing over.
    let tidy = || -> Result<Option<String>> {
        if let Some(wt) = own.and_then(|n| repo.find_worktree(&n).ok())
            && wt.validate().is_err()
        {
            let _ = wt.unlock();
            wt.prune(Some(WorktreePruneOptions::new().locked(true)))?;
        }
        for admin in repo.worktrees()?.iter().filter_map(|n| n.ok().flatten()) {
            if let Ok(wt) = repo.find_worktree(admin)
                && wt.validate().is_err()
                && matches!(wt.is_locked(), Ok(git2::WorktreeLockStatus::Unlocked))
            {
                wt.prune(None)?;
            }
        }
        if let Some(b) = &info.branch
            && *b == info.name
            && let Ok(mut branch) = repo.find_branch(b, BranchType::Local)
        {
            if delete_branch {
                branch.delete()?;
                info!("deleted branch {b}");
            } else {
                // Out of the way, so a new worktree of the same name starts fresh.
                let short = branch
                    .get()
                    .target()
                    .map(|o| o.to_string()[..7].to_string())
                    .unwrap_or_default();
                let name = format!("{b}-kept-{short}");
                branch.rename(&name, false)?;
                warn!("kept branch {b} as {name}: it has commits that aren't merged or pushed");
                return Ok(Some(name));
            }
        }
        Ok(None)
    };
    let kept = tidy().unwrap_or_else(|e| {
        warn!("{}: cleaning up git metadata: {e:#}", info.name);
        None
    });
    std::thread::spawn(move || {
        if let Err(e) = std::fs::remove_dir_all(&trash) {
            warn!("removing {}: {e}", trash.display());
        }
    });
    Ok(kept)
}

/// Prefix of a removed worktree moved next to itself (another volume than the git dir).
const SIBLING_TRASH: &str = ".localforest-trash.";

/// Delete what an interrupted removal left behind: the git dir's trash and hidden
/// siblings in `worktrees_dir`.
pub fn clean_trash(root: &Path, worktrees_dir: &Path) {
    let Ok(repo) = Repository::open(root) else {
        return;
    };
    let mut stale: Vec<PathBuf> = std::fs::read_dir(repo.commondir().join("localforest-trash"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    stale.extend(
        std::fs::read_dir(worktrees_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(SIBLING_TRASH))
            .map(|e| e.path()),
    );
    if stale.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for p in stale {
            if let Err(e) = std::fs::remove_dir_all(&p) {
                warn!("removing {}: {e}", p.display());
            }
        }
    });
}

/// Every process as (pid, parent pid, cwd, executable path).
#[cfg(target_os = "macos")]
fn processes() -> Vec<(i32, i32, Vec<u8>, Vec<u8>)> {
    let mut pids = vec![0i32; 16384];
    let n = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    pids.truncate(n as usize);
    pids.into_iter()
        .filter_map(|pid| {
            let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut bsd as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            if got != size {
                return None;
            }
            let mut vn: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDVNODEPATHINFO,
                    0,
                    (&mut vn as *mut libc::proc_vnodepathinfo).cast(),
                    size,
                )
            };
            let cwd = if got == size {
                unsafe { std::ffi::CStr::from_ptr(vn.pvi_cdir.vip_path.as_ptr().cast()) }
                    .to_bytes()
                    .to_vec()
            } else {
                Vec::new()
            };
            let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
            buf.truncate(len.max(0) as usize);
            Some((pid, bsd.pbi_ppid as i32, cwd, buf))
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn processes() -> Vec<(i32, i32, Vec<u8>, Vec<u8>)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            // pid (comm) state ppid ...
            let ppid = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            let link = |n: &str| {
                std::fs::read_link(e.path().join(n))
                    .map(|p| p.as_os_str().as_bytes().to_vec())
                    .unwrap_or_default()
            };
            Some((pid, ppid, link("cwd"), link("exe")))
        })
        .collect()
}

/// Processes running in `dir` (cwd or executable inside it: the BEAM, esbuild,
/// tailwind, node, ...) and all their descendants, except `keep`, this process and
/// their ancestors.
fn processes_in(dir: &Path, keep: &[i32]) -> Vec<i32> {
    let dir = dir.as_os_str().as_bytes();
    let inside = |p: &[u8]| p.starts_with(dir) && (p.len() == dir.len() || p[dir.len()] == b'/');
    let procs: Vec<(i32, i32, bool)> = processes()
        .into_iter()
        .map(|(p, pp, cwd, exe)| (p, pp, inside(&cwd) || inside(&exe)))
        .collect();
    let mut roots = keep.to_vec();
    roots.push(std::process::id() as i32);
    sweep(&procs, &roots)
}

/// Of `procs` as (pid, parent pid, runs inside the dir): those inside and their
/// descendants, except `keep` and their ancestors. The sweep doesn't go through a
/// spared process: its other children (MCP servers, hook shells) are only hit when
/// they themselves run inside.
fn sweep(procs: &[(i32, i32, bool)], keep: &[i32]) -> Vec<i32> {
    let parent: HashMap<i32, i32> = procs.iter().map(|(p, pp, _)| (*p, *pp)).collect();
    let spared = with_ancestors(&parent, keep);
    let mut hit: HashSet<i32> = procs
        .iter()
        .filter(|(p, _, inside)| *inside && !spared.contains(p) && *p > 1)
        .map(|(p, ..)| *p)
        .collect();
    loop {
        let before = hit.len();
        for (p, pp, _) in procs {
            if hit.contains(pp) && !spared.contains(p) {
                hit.insert(*p);
            }
        }
        if hit.len() == before {
            break;
        }
    }
    hit.into_iter().collect()
}

/// This process's ancestors, from the process table (sent along with removals so
/// the daemon can spare them even when it can't see them all).
pub fn ancestors() -> Vec<i32> {
    let parent: HashMap<i32, i32> = processes().iter().map(|(p, pp, ..)| (*p, *pp)).collect();
    let mut out: Vec<i32> = with_ancestors(&parent, &[std::process::id() as i32])
        .into_iter()
        .collect();
    out.push(unsafe { libc::getppid() });
    out
}

/// `pids` and every ancestor of each (a hook's shell runs under Claude Code, which
/// may sit in the worktree being removed).
fn with_ancestors(parent: &HashMap<i32, i32>, pids: &[i32]) -> HashSet<i32> {
    let mut out = HashSet::new();
    for &p in pids {
        let mut p = p;
        while p > 1 && out.insert(p) {
            p = parent.get(&p).copied().unwrap_or(0);
        }
    }
    out
}

/// SIGKILL everything running in `dir` (dev servers and their watchers): a dev server
/// needs no graceful shutdown, and left alive they'd write into the deleted worktree.
pub async fn kill_processes_in(dir: &Path, keep: &[i32]) {
    for _ in 0..3 {
        let pids = processes_in(dir, keep);
        if pids.is_empty() {
            return;
        }
        info!("killing {} process(es) in {}", pids.len(), dir.display());
        for p in &pids {
            unsafe { libc::kill(*p, libc::SIGKILL) };
        }
        // Catch anything forked meanwhile.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectSettings;
    use git2::{Signature, WorktreeAddOptions};
    use tempfile::TempDir;

    #[test]
    fn env_values_read_back_verbatim() {
        let values = [
            "postgres://u:pw@127.0.0.1:55432/db",
            "a $HOME ${HOME} $(id) `id` b",
            "it's",
            "say \"hi\" \\ back\\slash",
            "two\nlines",
            "it's $HOME\nand \"more\"\\n",
            "it's `id` $(id)",
            "cr\r\nlf",
            "",
            "#not a comment",
        ];
        assert_eq!(dotenv_quote("a $B"), "'a $B'");
        assert_eq!(dotenv_quote("it's $B\n"), "\"it's \\$B\\n\"");
        // Backticks in single-quoted pieces: `set -a; . .env` must not run them.
        assert_eq!(
            dotenv_quote("it's `id` $(id) \"q\" \\"),
            r#""it's "'`'"id"'`'" \$(id) \"q\" \\""#
        );
        let dir = TempDir::new().unwrap();
        let file = dir.path().join(".env");
        let text: String = values
            .iter()
            .enumerate()
            .map(|(i, v)| format!("K{i}={}\n", dotenv_quote(v)))
            .collect();
        std::fs::write(&file, text).unwrap();
        let parsed: HashMap<String, String> = dotenvy::from_path_iter(&file)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for (i, v) in values.iter().enumerate() {
            assert_eq!(parsed[&format!("K{i}")], *v, "value {i}");
        }
    }

    fn settings() -> ProjectSettings {
        ProjectSettings {
            name: Some("app".into()),
            port: 4000,
            remote: "origin".into(),
            base: None,
            worktrees_dir: ".claude/worktrees".into(),
            migrate: None,
            seed: None,
            setup: None,
            services: Default::default(),
            preview_ttl_hours: 48,
            no_sync: false,
            no_auto_remove: false,
        }
    }

    fn fixture() -> (TempDir, Project, Syncer) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("app");
        let repo = Repository::init(&root).unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join(".gitignore"), "cache/\n").unwrap();
        std::fs::create_dir(root.join("cache")).unwrap();
        std::fs::write(root.join("cache/big"), "built\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.txt")).unwrap();
        idx.add_path(Path::new(".gitignore")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = Signature::now("T", "t@example.com").unwrap();
        repo.commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let root = root.canonicalize().unwrap();
        let project = Project::new(root.clone(), settings());
        let syncer = Syncer {
            path: root,
            remote: "origin".into(),
            base: "main".into(),
            token: None,
            token_host: "github.com".into(),
        };
        (dir, project, syncer)
    }

    #[test]
    fn creates_lists_and_removes() {
        let (_d, project, syncer) = fixture();
        let path = create(&project, &syncer, "feat-x", None).unwrap();
        assert_eq!(std::fs::read_to_string(path.join("a.txt")).unwrap(), "a\n");
        // Ignored build caches are carried.
        assert_eq!(
            std::fs::read_to_string(path.join("cache/big")).unwrap(),
            "built\n"
        );
        assert!(!is_dirty(&path).unwrap());

        let infos = list(&project.root).unwrap();
        assert_eq!(infos.len(), 1);
        let info = &infos[0];
        assert_eq!(info.name, "feat-x");
        assert_eq!(info.branch.as_deref(), Some("feat-x"));
        assert!(matches!(
            safety(info, "origin", "main").unwrap(),
            Safety::Safe
        ));

        // A commit of its own needs a merged PR.
        let repo = Repository::open(&path).unwrap();
        std::fs::write(path.join("b.txt"), "b\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("b.txt")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = Signature::now("T", "t@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let pr_head = repo
            .commit(Some("HEAD"), &sig, &sig, "b", &tree, &[&parent])
            .unwrap();
        let info = &list(&project.root).unwrap()[0];
        assert!(matches!(
            safety(info, "origin", "main").unwrap(),
            Safety::NeedsMergedPr { .. }
        ));
        assert!(covered_by_pr(info, pr_head, "origin", "main").unwrap());

        std::fs::write(path.join("c.txt"), "dirty\n").unwrap();
        assert!(safety(info, "origin", "main").is_err());
        std::fs::remove_file(path.join("c.txt")).unwrap();

        assert_eq!(unpushed(info, "origin", "main").unwrap(), 1);
        remove_files(&project.root, info, true).unwrap();
        assert!(!path.exists());
        assert!(list(&project.root).unwrap().is_empty());
        let repo = Repository::open(&project.root).unwrap();
        assert!(repo.find_branch("feat-x", BranchType::Local).is_err());
        // Can be created again.
        create(&project, &syncer, "feat-x", None).unwrap();
    }

    #[test]
    fn counts_auto_merges_with_changes_of_their_own() {
        let d = TempDir::new().unwrap();
        let repo = Repository::init(d.path()).unwrap();
        let sig = Signature::now("T", "t@example.com").unwrap();
        let commit = |files: &[(&str, &str)], parents: &[Oid], msg: &str| {
            let mut tb = repo.treebuilder(None).unwrap();
            for (name, content) in files {
                tb.insert(name, repo.blob(content.as_bytes()).unwrap(), 0o100644)
                    .unwrap();
            }
            let tree = repo.find_tree(tb.write().unwrap()).unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|p| repo.find_commit(*p).unwrap())
                .collect();
            let parents: Vec<_> = parents.iter().collect();
            repo.commit(None, &sig, &sig, msg, &tree, &parents).unwrap()
        };
        let a = commit(&[("a", "1")], &[], "init");
        let own = commit(&[("a", "1"), ("b", "own")], &[a], "own");
        let main = commit(&[("a", "2")], &[a], "main");
        let msg = "Merge remote-tracking branch 'origin/main' into x";
        let clean = commit(&[("a", "2"), ("b", "own")], &[own, main], msg);
        assert_eq!(
            ahead(&repo, clean, &[own, main], "origin", "main").unwrap(),
            0
        );
        let evil = commit(
            &[("a", "2"), ("b", "own"), ("c", "extra")],
            &[own, main],
            msg,
        );
        assert_eq!(
            ahead(&repo, evil, &[own, main], "origin", "main").unwrap(),
            1
        );
        // Not what libgit2 would make, but every path is one side's as is (as a
        // `git merge` with other rename detection might do): nothing of its own.
        let other = commit(&[("a", "1"), ("b", "own")], &[own, main], msg);
        assert_eq!(
            ahead(&repo, other, &[own, main], "origin", "main").unwrap(),
            0
        );
        // A hand-resolved path, in neither side nor the clean merge: work.
        let resolved = commit(&[("a", "3"), ("b", "own")], &[own, main], msg);
        assert_eq!(
            ahead(&repo, resolved, &[own, main], "origin", "main").unwrap(),
            1
        );
    }

    #[test]
    fn pushed_commits_are_not_unpushed() {
        let (_d, project, syncer) = fixture();
        let path = create(&project, &syncer, "feat-x", None).unwrap();
        let info = list(&project.root).unwrap().remove(0);
        assert_eq!(unpushed(&info, "origin", "main").unwrap(), 0);
        let repo = Repository::open(&path).unwrap();
        let sig = Signature::now("T", "t@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let head = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "own",
                &parent.tree().unwrap(),
                &[&parent],
            )
            .unwrap();
        assert_eq!(unpushed(&info, "origin", "main").unwrap(), 1);
        repo.reference("refs/remotes/origin/feat-x", head, true, "push")
            .unwrap();
        assert_eq!(unpushed(&info, "origin", "main").unwrap(), 0);
    }

    #[test]
    fn lists_ignored_files_only_the_worktree_has() {
        let (_d, project, syncer) = fixture();
        let path = create(&project, &syncer, "feat-x", None).unwrap();
        // Carried from the primary (cache/), build caches: quiet.
        std::fs::create_dir(path.join("node_modules")).unwrap();
        std::fs::write(path.join("node_modules/x"), "").unwrap();
        let exclude = project.root.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, "*.local\n.env\n").unwrap();
        std::fs::write(project.root.join("same.local"), "x\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(path.join("same.local"), "x\n").unwrap();
        let fifo = CString::new(path.join("pipe.local").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        assert_eq!(
            ignored_files(&project.root, &path).unwrap(),
            Vec::<String>::new()
        );
        // Own files, edited copies, a carried dir with more in it, .env: reported.
        std::fs::write(path.join("secrets.local"), "mine\n").unwrap();
        std::fs::write(path.join("same.local"), "edit\n").unwrap();
        std::fs::write(path.join("cache/notes"), "mine\n").unwrap();
        std::fs::write(path.join(".env"), "KEY=1\n").unwrap();
        let mut files = ignored_files(&project.root, &path).unwrap();
        files.sort();
        assert_eq!(files, vec![".env", "cache/", "same.local", "secrets.local"]);
    }

    #[test]
    fn spares_whole_ancestor_chains() {
        // 1 <- 100 (claude) <- 200 (sh) <- 300 (localforest); 1 <- 400 <- 500
        let parent = HashMap::from([(100, 1), (200, 100), (300, 200), (400, 1), (500, 400)]);
        assert_eq!(
            with_ancestors(&parent, &[300, 500]),
            HashSet::from([100, 200, 300, 400, 500])
        );
        // Unknown pids are kept themselves.
        assert_eq!(with_ancestors(&parent, &[42]), HashSet::from([42]));
    }

    #[test]
    fn sweep_does_not_go_through_spared_processes() {
        // claude 100 (inside) <- sh 200 <- localforest 300; claude <- mcp 110 (outside)
        // <- 111; claude <- server 120 (inside) <- watcher 121; other 400 (outside).
        let procs = [
            (100, 1, true),
            (200, 100, false),
            (300, 200, false),
            (110, 100, false),
            (111, 110, false),
            (120, 100, true),
            (121, 120, false),
            (400, 1, false),
        ];
        let mut hit = sweep(&procs, &[300]);
        hit.sort();
        assert_eq!(hit, vec![120, 121]);
    }

    #[test]
    fn removal_prunes_stale_unlocked_but_not_locked_worktrees() {
        let (_d, project, syncer) = fixture();
        let away = create(&project, &syncer, "away", None).unwrap();
        let by_hand = create(&project, &syncer, "by-hand", None).unwrap();
        create(&project, &syncer, "gone", None).unwrap();
        let repo = Repository::open(&project.root).unwrap();
        // Locked on purpose, its volume unmounted: invalid, but not ours to prune.
        repo.find_worktree("away")
            .unwrap()
            .lock(Some("on a usb disk"))
            .unwrap();
        let infos = list(&project.root).unwrap();
        let find = |n: &str| infos.iter().find(|i| i.name == n).unwrap();
        assert!(is_locked(&project.root, find("away")));
        assert!(!is_locked(&project.root, find("gone")));
        std::fs::rename(&away, away.with_file_name("away-unmounted")).unwrap();
        std::fs::remove_dir_all(&by_hand).unwrap();

        remove_files(&project.root, find("gone"), true).unwrap();
        let wt = repo.find_worktree("away").unwrap();
        assert!(matches!(
            wt.is_locked().unwrap(),
            git2::WorktreeLockStatus::Locked(_)
        ));
        assert!(repo.find_worktree("gone").is_err());
        assert!(repo.find_worktree("by-hand").is_err());
    }

    #[test]
    fn cleans_up_trash_of_interrupted_removals() {
        let (_d, project, _) = fixture();
        let trash = project.root.join(".git/localforest-trash/old.123");
        let sibling = project
            .worktrees_dir()
            .join(format!("{SIBLING_TRASH}old.123"));
        let keep = project.worktrees_dir().join("feat-x");
        for d in [&trash, &sibling, &keep] {
            std::fs::create_dir_all(d.join("sub")).unwrap();
        }
        clean_trash(&project.root, &project.worktrees_dir());
        for _ in 0..100 {
            if !trash.exists() && !sibling.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!trash.exists() && !sibling.exists());
        assert!(keep.exists());
    }

    #[test]
    fn removal_can_keep_the_branch() {
        let (_d, project, syncer) = fixture();
        let path = create(&project, &syncer, "keep-me", None).unwrap();
        let info = list(&project.root).unwrap().remove(0);
        assert_eq!(unpushed(&info, "origin", "main").unwrap(), 0);
        std::fs::write(path.join("new.txt"), "x").unwrap();
        std::fs::write(path.join("a.txt"), "changed").unwrap();
        let mut dirty = uncommitted(&path).unwrap();
        dirty.sort();
        assert_eq!(dirty, vec!["a.txt", "new.txt"]);
        let repo = Repository::open(&project.root).unwrap();
        let head = repo.head().unwrap().target().unwrap().to_string();

        let kept = remove_files(&project.root, &info, false).unwrap().unwrap();
        assert_eq!(kept, format!("keep-me-kept-{}", &head[..7]));
        assert!(!info.path.exists());
        assert!(repo.find_branch(&kept, BranchType::Local).is_ok());
        assert!(repo.find_branch("keep-me", BranchType::Local).is_err());
    }

    #[test]
    fn knows_when_a_worktree_was_made() {
        let (_d, project, syncer) = fixture();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        create(&project, &syncer, "feat-x", None).unwrap();
        let info = list(&project.root).unwrap().remove(0);
        let t = created_at(&info).unwrap();
        assert!((before - 1..=before + 60).contains(&t), "{t} vs {before}");
        // Merged 10 minutes later: before. Within the clock-skew grace, or earlier: not.
        assert!(made_before(&info, t + 600).is_ok());
        assert!(made_before(&info, t + 60).is_err());
        assert!(made_before(&info, t - 3600).is_err());
    }

    #[test]
    fn copies_trees_keeping_mtimes_modes_and_links() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir_all(src.join("lib")).unwrap();
        std::fs::write(src.join("lib/a.beam"), "beam").unwrap();
        std::fs::write(src.join("run"), "#!/bin/sh").unwrap();
        std::fs::set_permissions(src.join("run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("lib/a.beam", src.join("link")).unwrap();
        let old = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(src.join("lib/a.beam"), old).unwrap();

        let dst = d.path().join("dst");
        copy_tree(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("lib/a.beam")).unwrap(),
            "beam"
        );
        let meta = std::fs::metadata(dst.join("lib/a.beam")).unwrap();
        assert_eq!(filetime::FileTime::from_last_modification_time(&meta), old);
        let mode = std::fs::metadata(dst.join("run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_link(dst.join("link")).unwrap(),
            Path::new("lib/a.beam")
        );
    }

    #[test]
    fn plain_git_worktree_gets_caches_and_env() {
        let (_d, project, _) = fixture();
        let root = &project.root;
        // Per-checkout index of the primary: must not follow.
        std::fs::create_dir(root.join(".dexter")).unwrap();
        std::fs::write(root.join(".dexter/index.db"), root.display().to_string()).unwrap();
        // What `git worktree add` does: a full checkout, no ignored files.
        let repo = Repository::open(root).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let branch = repo.branch("plain", &head, false).unwrap().into_reference();
        let path = root.join(".claude/worktrees/plain");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        repo.worktree(
            "plain",
            &path,
            Some(WorktreeAddOptions::new().reference(Some(&branch))),
        )
        .unwrap();
        let info = list(root)
            .unwrap()
            .into_iter()
            .find(|i| i.name == "plain")
            .unwrap();
        assert!(!initializing(root, &info));
        assert!(!info.path.join("cache").exists());

        assert!(carry_caches(root, &info).unwrap());
        assert_eq!(
            std::fs::read_to_string(info.path.join("cache/big")).unwrap(),
            "built\n"
        );
        assert!(!info.path.join(".dexter").exists());
        assert!(!is_dirty(&info.path).unwrap());
        // Has them now: nothing more to do.
        assert!(!carry_caches(root, &info).unwrap());

        // .env: gitignored via info/exclude, block on top, cloned keys disabled.
        std::fs::write(
            info.path.join(".env"),
            "DATABASE_URL=postgres://primary\nOTHER=1\n",
        )
        .unwrap();
        let env = vec![
            ("DATABASE_URL".to_string(), "postgres://wt".to_string()),
            ("PORT".to_string(), "20000".to_string()),
        ];
        write_env(&info.path, &env).unwrap();
        write_env(&info.path, &env).unwrap();
        let text = std::fs::read_to_string(info.path.join(".env")).unwrap();
        assert_eq!(
            text,
            format!(
                "{ENV_BEGIN}\nDATABASE_URL='postgres://wt'\nPORT='20000'\n{ENV_END}\n\
                 # overridden by localforest: DATABASE_URL=postgres://primary\nOTHER=1\n"
            )
        );
        let wt = Repository::open(&info.path).unwrap();
        assert!(wt.status_should_ignore(Path::new(".env")).unwrap());
        assert!(!is_dirty(&info.path).unwrap());
    }
}
