//! Worktree lifecycle, following git-cow: `git worktree add --no-checkout` (here via
//! libgit2), then fill it with copy-on-write clones of the primary checkout, build
//! caches included, so a new worktree costs ~0 disk and needs no rebuild.
//! Removal refuses to lose work unless forced.

use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use git2::{BranchType, Oid, Repository, Status, StatusOptions, WorktreePruneOptions};
use tracing::{info, warn};

use crate::{
    config::{self, Project, valid_label, worktree_label},
    cow::{self, PER_CHECKOUT, populated_marker},
    sync::Syncer,
};

mod procs;

pub use self::procs::{
    MARKER, ancestors, drop_marker, fd_open_on, kill_processes_in, marker_path, processes_left,
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
        let name = worktree_label(admin);
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

/// The worktree's base port, recorded in its git admin dir (gone with it).
const PORT_FILE: &str = "lazy-cow-tree-port";

/// Admin dir of the worktree at `path`, from its `.git` file (`gitdir: <dir>`),
/// without opening the repository.
fn admin_dir(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path.join(".git")).ok()?;
    let dir = Path::new(text.strip_prefix("gitdir:")?.trim());
    Some(path.join(dir))
}

/// The base port recorded in the worktree's git admin dir.
pub fn recorded_port(path: &Path) -> Option<u16> {
    let p: u16 = std::fs::read_to_string(admin_dir(path)?.join(PORT_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (config::WORKTREE_PORTS.contains(&p) && p.is_multiple_of(10)).then_some(p)
}

/// First free slot from `i`'s hashed one on (linear probing, wrapping): no other
/// worktree's, and no other process listens on its ports.
fn probe(project: &Project, i: &Info, used: &HashSet<u16>) -> u16 {
    let first = config::worktree_port(&project.name, &i.name);
    let slots = config::WORKTREE_PORTS.len() as u16 / 10;
    (0..slots)
        .map(|k| {
            let slot = ((first - config::WORKTREE_PORTS.start) / 10 + k) % slots;
            config::WORKTREE_PORTS.start + slot * 10
        })
        .find(|&p| {
            !used.contains(&p)
                && project
                    .checkout_on(Some(&i.name), i.path.clone(), p)
                    .used_ports()
                    .into_iter()
                    .all(port_free)
        })
        .unwrap_or(first)
}

/// Nothing listens on 127.0.0.1:`port` (binding it works).
pub fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The primary checkout's base port, recorded in its git dir as `<setting> <port>`.
const PRIMARY_PORT_FILE: &str = "lazy-cow-tree-primary-port";

/// The primary checkout's base port: the one recorded for setting `requested`
/// (`assign_primary_port`), else `requested`.
pub fn primary_port(root: &Path, requested: u16) -> u16 {
    std::fs::read_to_string(root.join(".git").join(PRIMARY_PORT_FILE))
        .ok()
        .and_then(|t| {
            let (setting, port) = t.trim().split_once(' ')?;
            (setting.parse() == Ok(requested)).then(|| port.parse().ok())?
        })
        .unwrap_or(requested)
}

/// Pick and record the base port of `project`'s primary checkout, as devenv's
/// `ports.*.allocate` does: its setting (or the port recorded for it), unless one of
/// its services' ports is another registered project's or, when `check_listeners`,
/// another process listens there; then the next block of 10 above it. With `strict`
/// (devenv's `strict_ports`) a taken port is an error instead.
pub fn assign_primary_port(
    project: &Project,
    others: &[Project],
    check_listeners: bool,
    strict: bool,
) -> Result<u16> {
    let requested = project.settings.port;
    let taken: HashMap<u16, &str> = others
        .iter()
        .flat_map(|o| {
            o.checkout(None, o.root.clone())
                .used_ports()
                .into_iter()
                .map(|p| (p, o.name.as_str()))
        })
        .collect();
    let clash = |base: u16, listeners: bool| -> Option<String> {
        let ports = project
            .checkout_on(None, project.root.clone(), base)
            .used_ports();
        ports.iter().find_map(|p| {
            taken
                .get(p)
                .map(|o| format!("port {p} is project {o}'s"))
                .or_else(|| (listeners && !port_free(*p)).then(|| format!("port {p} is in use")))
        })
    };
    let current = primary_port(&project.root, requested);
    let port = match clash(current, check_listeners) {
        None => current,
        Some(why) if strict => match clash(requested, true) {
            None => requested,
            Some(_) => bail!(
                "{}: {why}, and strict_ports (devenv.yaml) keeps it from moving: set lazy-cow-tree.port",
                project.name
            ),
        },
        Some(why) => {
            let fits = |b: &u32| {
                let range =
                    u32::from(config::WORKTREE_PORTS.start)..u32::from(config::WORKTREE_PORTS.end);
                *b + 9 <= u32::from(u16::MAX) && !range.contains(b) && !range.contains(&(*b + 9))
            };
            let base = (1..1000u32)
                .map(|k| u32::from(requested) + 10 * k)
                .filter(fits)
                .find(|&b| clash(b as u16, true).is_none())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}: {why}, and no free block of ports above it",
                        project.name
                    )
                })? as u16;
            info!(
                "{}: {why}; its primary checkout moves to port {base}",
                project.name
            );
            base
        }
    };
    let file = project.root.join(".git").join(PRIMARY_PORT_FILE);
    if port == requested {
        let _ = std::fs::remove_file(file);
    } else if port != current {
        std::fs::write(file, format!("{requested} {port}\n"))?;
    }
    Ok(port)
}

/// Base port of every worktree of `projects` (all registered ones: the daemon's, or
/// `state.json` for the shell hook), by path. Recorded ports hold; the others
/// get, in order of (project root, name), the first slot from their hashed one that
/// no primary, recorded or earlier worktree has. Pure: the daemon and `lazy-cow-tree
/// env` get the same answer from the same projects and admin dirs.
pub fn plan_ports(projects: &[Project]) -> Result<Vec<(PathBuf, u16, bool)>> {
    let mut used: HashSet<u16> = projects
        .iter()
        .map(|p| primary_port(&p.root, p.settings.port))
        .collect();
    let mut recorded = Vec::new();
    let mut open = Vec::new();
    let mut projects: Vec<&Project> = projects.iter().collect();
    projects.sort_by(|a, b| a.root.cmp(&b.root));
    for p in projects {
        let mut infos = list(&p.root)?;
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        for i in infos {
            match recorded_port(&i.path) {
                Some(port) => {
                    if !used.insert(port) {
                        warn!(
                            "worktree {} shares port {port} with another",
                            i.path.display()
                        );
                    }
                    recorded.push((i.path, port, true));
                }
                None => open.push((p, i)),
            }
        }
    }
    for (project, i) in open {
        let port = probe(project, &i, &used);
        used.insert(port);
        recorded.push((i.path, port, false));
    }
    Ok(recorded)
}

/// Serializes recording (the daemon's projects reconcile concurrently).
static RECORDING: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// `plan_ports` and record the new ones, so they never move while their worktree
/// lives (daemon only). Blocking: call from `spawn_blocking`.
pub fn assign_ports(projects: &[Project]) -> Result<Vec<(PathBuf, u16)>> {
    let _g = RECORDING.lock();
    let plan = plan_ports(projects)?;
    for (path, port, recorded) in &plan {
        if *recorded {
            continue;
        }
        let Some(admin) = admin_dir(path) else {
            continue;
        };
        std::fs::write(admin.join(PORT_FILE), format!("{port}\n"))?;
    }
    Ok(plan.into_iter().map(|(p, port, _)| (p, port)).collect())
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

/// The worktree name (directory, hostname, databases) of branch `branch`: its `/`s
/// as `-` (`feat/login` -> `feat-login`). Two branches may share one, so `create`
/// refuses a name another branch's worktree has.
pub fn name_of_branch(branch: &str) -> Result<String> {
    let name = branch.replace('/', "-");
    if branch.split('/').any(str::is_empty) || !valid_label(&name) {
        bail!(
            "{branch}: use at most 32 of a-z, 0-9, '-' and '/' ('/' becomes '-' in its hostname)"
        );
    }
    Ok(name)
}

/// Create a worktree on a new branch `branch` from `base` (default: the freshly
/// fetched base branch, or the local one when it only adds commits on top), named
/// `name_of_branch`.
pub fn create(
    project: &Project,
    syncer: &Syncer,
    branch: &str,
    base: Option<&str>,
) -> Result<PathBuf> {
    let name = &name_of_branch(branch)?;
    let root = &project.root;
    let dir = project.worktrees_dir();
    let path = dir.join(name);
    let infos = list(root)?;
    // `feat/login` and `feat-login` would share hostnames and databases.
    if let Some(i) = infos.iter().find(|i| i.name == *name)
        && let Some(other) = i.branch.as_deref().filter(|b| *b != branch)
    {
        bail!(
            "worktree {name} (branch {other}) would share its name, hostnames and databases \
             with branch {branch}; pick another name"
        );
    }
    if path.join(".git").exists() {
        let existing = path.canonicalize()?;
        // Only the worktree of this very name: never hand out another's checkout.
        match infos.iter().find(|i| i.path == existing) {
            Some(i) if i.name == *name => {
                info!("worktree {name} already exists");
                return Ok(existing);
            }
            Some(i) => bail!("{} is worktree {}, not {name}", path.display(), i.name),
            None => bail!("{} exists and is not a worktree", path.display()),
        }
    }
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    // E.g. git numbered a `feat` elsewhere's admin dir `feat1`.
    if let Some(i) = infos.iter().find(|i| i.name == *name) {
        bail!(
            "worktree name {name} is taken by {} (its git admin dir); pick another name",
            i.path.display()
        );
    }

    let repo = Repository::open(root)?;
    let branch_exists = repo.find_branch(branch, BranchType::Local).is_ok();
    if branch_exists {
        if base.is_some() {
            bail!("branch {branch} already exists; drop the base to check it out");
        }
        info!("checking out existing branch {branch}");
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
        repo.branch(branch, &commit, false)?;
    }

    // A stale admin dir (worktree deleted by hand) would make the add fail.
    if let Ok(wt) = repo.find_worktree(name)
        && wt.validate().is_err()
    {
        wt.prune(Some(WorktreePruneOptions::new().working_tree(true)))?;
    }
    std::fs::create_dir_all(&dir)?;
    add_no_checkout(root, name, &path, branch)?;
    cow::populate(root, &path, name)?;
    Ok(path.canonicalize()?)
}

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

/// Written in the worktree's git admin dir once its setup command succeeded.
pub const SETUP_MARKER: &str = "lazy-cow-tree-setup";

/// When provisioning finished: the newest of the populated and setup markers.
fn provisioned_at(path: &Path) -> Option<SystemTime> {
    let admin = Repository::open(path).ok()?.path().to_path_buf();
    [populated_marker(path).ok()?, admin.join(SETUP_MARKER)]
        .iter()
        .filter_map(|m| std::fs::metadata(m).ok()?.modified().ok())
        .max()
}

/// Last modified or created/moved here (ctime).
fn changed_at(m: &std::fs::Metadata) -> SystemTime {
    use std::os::unix::fs::MetadataExt;
    let ctime = SystemTime::UNIX_EPOCH
        + Duration::new(
            u64::try_from(m.ctime()).unwrap_or_default(),
            u32::try_from(m.ctime_nsec()).unwrap_or_default(),
        );
    m.modified().map_or(ctime, |t| t.max(ctime))
}

/// Nothing at `p` (the file, or a directory and what's in it) changed after `t`.
/// Gives up (false) past a few thousand entries rather than walk a big tree.
fn unchanged_since(p: &Path, t: SystemTime) -> bool {
    let mut budget = 5000usize;
    let mut stack = vec![p.to_path_buf()];
    while let Some(q) = stack.pop() {
        let Ok(m) = std::fs::symlink_metadata(&q) else {
            return false;
        };
        if changed_at(&m) > t {
            return false;
        }
        if m.is_dir() {
            let Ok(entries) = std::fs::read_dir(&q) else {
                return false;
            };
            for e in entries.flatten() {
                if budget == 0 {
                    return false;
                }
                budget -= 1;
                stack.push(e.path());
            }
        }
    }
    true
}

/// Ignored paths lazy-cow-tree carried into the worktree (listed in its populated
/// marker) or the primary's `.worktreeinclude` names (what git-cow carries).
struct Carried {
    paths: Vec<String>,
    include: Option<ignore::gitignore::Gitignore>,
}

impl Carried {
    fn load(root: &Path, path: &Path) -> Carried {
        let include = root.join(".worktreeinclude");
        Carried {
            paths: populated_marker(path)
                .ok()
                .and_then(|m| std::fs::read_to_string(m).ok())
                .unwrap_or_default()
                .lines()
                .map(|l| l.trim().trim_end_matches('/').to_string())
                .filter(|l| !l.is_empty())
                .collect(),
            include: include
                .is_file()
                .then(|| ignore::gitignore::Gitignore::new(&include).0),
        }
    }

    /// `rel`: relative to the worktree, without a trailing `/`.
    fn contains(&self, rel: &str, is_dir: bool) -> bool {
        self.paths.iter().any(|c| {
            rel == c
                || rel
                    .strip_prefix(c.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        }) || self
            .include
            .as_ref()
            .is_some_and(|i| i.matched_path_or_any_parents(rel, is_dir).is_ignore())
    }
}

/// Gitignored paths in the worktree at `path` that removal would delete for good:
/// not build caches or per-checkout indexes, not what lazy-cow-tree carried or wrote
/// (`.env` holding only its block and the primary's), not what was already there
/// when provisioning (setup) finished, and not copies of the primary checkout at
/// `root` (same file contents; directories with the same files and sizes).
/// Sockets and fifos are skipped.
pub fn ignored_files(root: &Path, path: &Path) -> Result<Vec<String>> {
    let carried = Carried::load(root, path);
    let provisioned = provisioned_at(path);
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
                && !carried.contains(rel, p.ends_with('/'))
                && !provisioned.is_some_and(|t| unchanged_since(&path.join(rel), t))
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
/// branch that lazy-cow-tree made (they add nothing of the branch's own). One with that
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
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<HashMap<Oid, bool>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().get(&c.id()) {
        return *v;
    }
    let clean = merge_adds_nothing(repo, c).unwrap_or(false);
    cache.lock().insert(c.id(), clean);
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
    let trash_dir = repo.commondir().join("lazy-cow-tree-trash");
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
const SIBLING_TRASH: &str = ".lazy-cow-tree-trash.";

/// Delete what an interrupted removal left behind: the git dir's trash and hidden
/// siblings in `worktrees_dir`.
pub fn clean_trash(root: &Path, worktrees_dir: &Path) {
    let Ok(repo) = Repository::open(root) else {
        return;
    };
    let mut stale: Vec<PathBuf> = std::fs::read_dir(repo.commondir().join("lazy-cow-tree-trash"))
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
#[cfg(test)]
mod tests;
