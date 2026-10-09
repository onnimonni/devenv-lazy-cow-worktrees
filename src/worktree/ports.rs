//! Ports: each worktree's base port (hashed from its name, then probed for one no
//! other worktree or process has; recorded in its git admin dir) and the primary's.

use super::*;

/// The worktree's base port, recorded in its git admin dir (gone with it).
pub(super) const PORT_FILE: &str = "lazy-cow-tree-port";

/// Admin dir of the worktree at `path`, from its `.git` file (`gitdir: <dir>`),
/// without opening the repository.
pub(super) fn admin_dir(path: &Path) -> Option<PathBuf> {
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
pub(super) fn probe(project: &Project, i: &Info, used: &HashSet<u16>) -> u16 {
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
pub(super) const PRIMARY_PORT_FILE: &str = "lazy-cow-tree-primary-port";

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
