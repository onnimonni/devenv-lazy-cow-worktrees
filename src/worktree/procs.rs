//! Processes running in a worktree: found by working directory or executable, and
//! killed with their descendants when it is removed.

use super::*;

/// Every process as (pid, parent pid, cwd, executable path).
#[cfg(target_os = "macos")]
pub(super) fn processes() -> Vec<(i32, i32, Vec<u8>, Vec<u8>)> {
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
pub(super) fn processes() -> Vec<(i32, i32, Vec<u8>, Vec<u8>)> {
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
pub(super) fn processes_in(dir: &Path, keep: &[i32]) -> Vec<i32> {
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
pub(super) fn sweep(procs: &[(i32, i32, bool)], keep: &[i32]) -> Vec<i32> {
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
pub(super) fn with_ancestors(parent: &HashMap<i32, i32>, pids: &[i32]) -> HashSet<i32> {
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
