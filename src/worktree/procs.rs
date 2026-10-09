//! Processes running in a worktree: found by working directory, executable or the
//! marker the shell hook opens (`MARKER`), and killed with their descendants when it
//! is removed.

use super::*;

/// What the shell hook keeps open in every shell inside a worktree, as `<fd>:<path>`:
/// an fd on the worktree's git admin dir (`marker_path`: nothing else keeps that one
/// open), inherited by everything started there, even what double-forks, `setsid`s
/// and `cd /`s away (node, bun, python and erlang close it in their children: on macOS
/// the devenv module's process-marker.c keeps it, on Linux `DEVENV_ROOT` in their
/// environment still tells).
pub const MARKER: &str = "WORKTREE_PROCESS_MARKER";

/// The marker uses fds from here up (bash: 213, zsh: its first free one >= 10).
const MARKER_MIN_FD: i32 = 10;

/// What a shell in `worktree` keeps its marker open on: its git admin dir
/// (`<common dir>/worktrees/<name>`), which `git worktree move` and `repair` leave as it
/// is (they rewrite `<worktree>/.git`). None for the primary checkout, or no worktree.
pub fn marker_path(worktree: &Path) -> Option<PathBuf> {
    let repo = Repository::open(worktree).ok()?;
    if !repo.is_worktree() {
        return None;
    }
    // As the kernel names it: /tmp is /private/tmp on macOS.
    repo.path().canonicalize().ok()
}

/// The marker in this process's environment, as (fd, path).
pub fn marker() -> Option<(i32, String)> {
    let m = std::env::var(MARKER).ok()?;
    let (fd, path) = m.split_once(':')?;
    Some((fd.parse().ok()?, path.to_string()))
}

/// Whether this process's `fd` is open on `path` (same file).
pub fn fd_open_on(fd: i32, path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return false;
    }
    std::fs::metadata(path)
        .is_ok_and(|m| m.dev() == st.st_dev as u64 && m.ino() == st.st_ino as u64)
}

/// Close the marker this process inherited (the daemon, started from a shell in a
/// worktree: what it starts for other checkouts mustn't look started in that one).
pub fn drop_marker() {
    if let Some((fd, path)) = marker()
        && fd_open_on(fd, &path)
    {
        unsafe { libc::close(fd) };
    }
}

/// A running process.
pub(super) struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub cwd: Vec<u8>,
    pub exe: Vec<u8>,
    /// When it started (pids are reused: checked again before killing).
    pub start: u64,
}

/// Every process.
#[cfg(target_os = "macos")]
pub(super) fn processes() -> Vec<Proc> {
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
    pids.into_iter().filter_map(process).collect()
}

/// One process.
#[cfg(target_os = "macos")]
pub(super) fn process(pid: i32) -> Option<Proc> {
    let bsd = bsdinfo(pid)?;
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
        c_path(&vn.pvi_cdir.vip_path)
    } else {
        Vec::new()
    };
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    buf.truncate(len.max(0) as usize);
    Some(Proc {
        pid,
        ppid: bsd.pbi_ppid as i32,
        cwd,
        exe: buf,
        start: start_of(&bsd),
    })
}

/// sys/proc_info.h's `struct socket_fdinfo` (not in libc), up to the TCP ports: a
/// `proc_fileinfo`, then `socket_info` whose `soi_proto` union is 528 bytes.
#[cfg(target_os = "macos")]
#[repr(C)]
struct SocketFdInfo {
    pfi: [u8; 24],
    /// `vinfo_stat`.
    soi_stat: [u64; 17],
    soi_so: u64,
    soi_pcb: u64,
    soi_type: i32,
    soi_protocol: i32,
    soi_family: i32,
    /// options, linger, state, qlen, incqlen, qlimit, timeo, error.
    soi_shorts: [u16; 8],
    soi_oobmark: u32,
    /// `sockbuf_info` rcv and snd.
    soi_bufs: [u32; 12],
    soi_kind: i32,
    rfu_1: u32,
    /// For TCP: `in_sockinfo`'s foreign then local port first (network order, low 16 bits).
    soi_proto: [u64; 66],
}

#[cfg(target_os = "macos")]
const PROC_PIDFDSOCKETINFO: i32 = 3;
#[cfg(target_os = "macos")]
const SOCKINFO_TCP: i32 = 2;

/// The process whose TCP connection from 127.0.0.1:`client_port` reaches local
/// `server_port` (a client of the daemon's proxies).
#[cfg(target_os = "macos")]
pub fn tcp_client_pid(client_port: u16, server_port: u16) -> Option<i32> {
    let mut pids = vec![0i32; 16384];
    let n = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    pids.truncate(n.max(0) as usize);
    let port = |v: i32| (v as u32 as u16).swap_bytes();
    pids.into_iter().find(|&pid| {
        fds_of(pid)
            .iter()
            .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
            .any(|f| {
                let mut info: SocketFdInfo = unsafe { std::mem::zeroed() };
                let size = std::mem::size_of::<SocketFdInfo>() as i32;
                let got = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        f.proc_fd,
                        PROC_PIDFDSOCKETINFO,
                        (&mut info as *mut SocketFdInfo).cast(),
                        size,
                    )
                };
                let words = info.soi_proto[0];
                let (fport, lport) = (words as u32 as i32, (words >> 32) as u32 as i32);
                got == size
                    && info.soi_kind == SOCKINFO_TCP
                    && port(lport) == client_port
                    && port(fport) == server_port
            })
    })
}

/// `pid`'s open fds.
#[cfg(target_os = "macos")]
fn fds_of(pid: i32) -> Vec<libc::proc_fdinfo> {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    let size =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    // Room for a few opened meanwhile.
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(size as usize / entry + 16);
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (fds.capacity() * entry) as i32,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    unsafe { fds.set_len(got as usize / entry) };
    fds
}

#[cfg(target_os = "macos")]
fn bsdinfo(pid: i32) -> Option<libc::proc_bsdinfo> {
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
    (got == size).then_some(bsd)
}

#[cfg(target_os = "macos")]
fn start_of(bsd: &libc::proc_bsdinfo) -> u64 {
    bsd.pbi_start_tvsec * 1_000_000 + bsd.pbi_start_tvusec
}

#[cfg(target_os = "macos")]
fn c_path(p: &[[libc::c_char; 32]; 32]) -> Vec<u8> {
    unsafe { std::ffi::CStr::from_ptr(p.as_ptr().cast()) }
        .to_bytes()
        .to_vec()
}

/// When `pid` started, if it runs.
#[cfg(target_os = "macos")]
pub(super) fn start_time(pid: i32) -> Option<u64> {
    bsdinfo(pid).map(|b| start_of(&b))
}

/// sys/proc_info.h's `struct vnode_fdinfowithpath` (not in libc).
#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfoWithPath {
    // struct proc_fileinfo
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
    pvip: libc::vnode_info_path,
}

#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEPATHINFO: i32 = 2;

/// Paths of the files `pid` holds open on fds a marker can be on.
#[cfg(target_os = "macos")]
fn open_files(pid: i32) -> Vec<Vec<u8>> {
    fds_of(pid)
        .iter()
        .filter(|f| f.proc_fd >= MARKER_MIN_FD && f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .filter_map(|f| {
            let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<VnodeFdInfoWithPath>() as i32;
            let got = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    f.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    (&mut info as *mut VnodeFdInfoWithPath).cast(),
                    size,
                )
            };
            (got == size).then(|| c_path(&info.pvip.vip_path))
        })
        .collect()
}

/// Its command line, as `ps` shows it.
#[cfg(target_os = "macos")]
pub(super) fn command(pid: i32) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let len = mib.len() as u32;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            len,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buf = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            len,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(size);
    // argc, the executable's path, NUL padding, then argc arguments (then the
    // environment, empty for other processes' since macOS 26).
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = &buf[4..];
    let exe_end = rest.iter().position(|&b| b == 0)?;
    let args = rest[exe_end..].iter().position(|&b| b != 0)? + exe_end;
    Some(join_args(rest[args..].split(|&b| b == 0).take(argc)))
}

/// Every process.
#[cfg(not(target_os = "macos"))]
pub(super) fn processes() -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let (ppid, start) = stat(pid)?;
            let link = |n: &str| {
                std::fs::read_link(e.path().join(n))
                    .map(|p| p.as_os_str().as_bytes().to_vec())
                    .unwrap_or_default()
            };
            Some(Proc {
                pid,
                ppid,
                cwd: link("cwd"),
                exe: link("exe"),
                start,
            })
        })
        .collect()
}

/// One process.
#[cfg(not(target_os = "macos"))]
pub(super) fn process(pid: i32) -> Option<Proc> {
    let (ppid, start) = stat(pid)?;
    let link = |n: &str| {
        std::fs::read_link(format!("/proc/{pid}/{n}"))
            .map(|p| p.as_os_str().as_bytes().to_vec())
            .unwrap_or_default()
    };
    Some(Proc {
        pid,
        ppid,
        cwd: link("cwd"),
        exe: link("exe"),
        start,
    })
}

/// The process whose TCP connection from 127.0.0.1:`client_port` reaches local
/// `server_port` (a client of the daemon's proxies): the socket's inode in
/// /proc/net/tcp{,6}, then the process holding it.
#[cfg(not(target_os = "macos"))]
pub fn tcp_client_pid(client_port: u16, server_port: u16) -> Option<i32> {
    let port = |a: &str| u16::from_str_radix(a.rsplit_once(':')?.1, 16).ok();
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"].iter().find_map(|f| {
        std::fs::read_to_string(f)
            .ok()?
            .lines()
            .skip(1)
            .find_map(|l| {
                let cols: Vec<&str> = l.split_whitespace().collect();
                (port(cols.get(1)?)? == client_port && port(cols.get(2)?)? == server_port)
                    .then(|| cols.get(9).map(|i| i.to_string()))?
            })
    })?;
    let want = format!("socket:[{inode}]");
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid: i32 = e.file_name().to_str()?.parse().ok()?;
        (std::fs::read_dir(e.path().join("fd"))
            .ok()?
            .flatten()
            .any(|f| std::fs::read_link(f.path()).is_ok_and(|l| l.as_os_str() == want.as_str())))
        .then_some(pid)
    })
}

/// (parent pid, start time) from /proc/<pid>/stat: pid (comm) state ppid ... starttime (22nd).
#[cfg(not(target_os = "macos"))]
fn stat(pid: i32) -> Option<(i32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

/// When `pid` started, if it runs.
#[cfg(not(target_os = "macos"))]
pub(super) fn start_time(pid: i32) -> Option<u64> {
    stat(pid).map(|(_, start)| start)
}

/// Paths of the files `pid` holds open on fds a marker can be on.
#[cfg(not(target_os = "macos"))]
fn open_files(pid: i32) -> Vec<Vec<u8>> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    fds.flatten()
        .filter(|f| {
            f.file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
                .is_some_and(|fd| fd >= MARKER_MIN_FD)
        })
        .filter_map(|f| std::fs::read_link(f.path()).ok())
        .map(|p| p.as_os_str().as_bytes().to_vec())
        .collect()
}

/// Its DEVENV_ROOT (what the shell hook and the daemon's services set in a worktree),
/// readable for the same user's processes.
#[cfg(not(target_os = "macos"))]
fn devenv_root(pid: i32) -> Option<Vec<u8>> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    env.split(|&b| b == 0)
        .find_map(|v| v.strip_prefix(b"DEVENV_ROOT=").map(<[u8]>::to_vec))
}

/// Its command line, as `ps` shows it.
#[cfg(not(target_os = "macos"))]
pub(super) fn command(pid: i32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
    (!raw.is_empty()).then(|| join_args(raw.split(|&b| b == 0)))
}

fn join_args<'a>(args: impl Iterator<Item = &'a [u8]>) -> String {
    args.map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `p` runs inside `dir`: its working directory or executable is in it, it
/// holds the marker of a shell there (open on `marker`), or on Linux its DEVENV_ROOT
/// is in it.
fn runs_inside(p: &Proc, dir: &[u8], marker: &[u8]) -> bool {
    let inside = |p: &[u8]| p.starts_with(dir) && (p.len() == dir.len() || p[dir.len()] == b'/');
    if inside(&p.cwd) || inside(&p.exe) {
        return true;
    }
    #[cfg(not(target_os = "macos"))]
    if devenv_root(p.pid).is_some_and(|r| inside(&r)) {
        return true;
    }
    !marker.is_empty() && open_files(p.pid).iter().any(|f| f == marker)
}

/// Which of `dirs` (worktrees) `pid` runs inside (`runs_inside`), with its command.
pub fn pid_inside(pid: i32, dirs: &[PathBuf]) -> Option<(usize, String)> {
    let p = process(pid)?;
    let i = dirs.iter().position(|d| {
        let marker = marker_path(d).unwrap_or_default();
        runs_inside(&p, d.as_os_str().as_bytes(), marker.as_os_str().as_bytes())
    })?;
    let cmd = command(pid).unwrap_or_else(|| String::from_utf8_lossy(&p.exe).into_owned());
    Some((i, cmd))
}

/// Processes running in `dir` (`runs_inside`: the BEAM, esbuild, tailwind, node, what a
/// shell there started and detached, ...) and all their descendants, as (pid, start
/// time, executable), except `keep`, this process and their ancestors, and with
/// `spare_ours` what lazy-cow-tree runs (the daemon's services, which it stops itself;
/// `lazy-cow-tree lsp`'s language servers).
pub(super) fn processes_in(dir: &Path, keep: &[i32], spare_ours: bool) -> Vec<(i32, u64, Vec<u8>)> {
    let marker = marker_path(dir).unwrap_or_default();
    let marker = marker.as_os_str().as_bytes();
    let dir = dir.as_os_str().as_bytes();
    let all = processes();
    let me = std::process::id() as i32;
    let procs: Vec<(i32, i32, bool)> = all
        .iter()
        .map(|p| (p.pid, p.ppid, p.pid != me && runs_inside(p, dir, marker)))
        .collect();
    let mut roots = keep.to_vec();
    roots.push(me);
    let mut hit = sweep(&procs, &roots);
    if spare_ours {
        let ours: Vec<i32> = all
            .iter()
            .filter(|p| is_ours(&p.exe))
            .map(|p| p.pid)
            .collect();
        let parent: HashMap<i32, i32> = procs.iter().map(|(p, pp, _)| (*p, *pp)).collect();
        hit.retain(|&p| {
            let line = with_ancestors(&parent, &[p]);
            !ours.iter().any(|o| line.contains(o))
        });
    }
    let by_pid: HashMap<i32, &Proc> = all.iter().map(|p| (p.pid, p)).collect();
    hit.into_iter()
        .map(|p| (p, by_pid[&p].start, by_pid[&p].exe.clone()))
        .collect()
}

/// Whether `exe` is a lazy-cow-tree binary (any build of it).
fn is_ours(exe: &[u8]) -> bool {
    let name = exe.rsplit(|&b| b == b'/').next().unwrap_or_default();
    name == b"lazy-cow-tree" || name == b"lazy-cow-tree-devenv-proxy"
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
    let parent: HashMap<i32, i32> = processes().iter().map(|p| (p.pid, p.ppid)).collect();
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

/// A process left running in a worktree.
pub struct Left {
    pub pid: i32,
    pub command: String,
}

/// What still runs in `dir`, but `keep`, their ancestors and what lazy-cow-tree runs
/// (the daemon's services: it stops them itself; language servers).
pub fn processes_left(dir: &Path, keep: &[i32]) -> Vec<Left> {
    let mut left: Vec<Left> = processes_in(dir, keep, true)
        .into_iter()
        .map(|(pid, _, exe)| Left {
            pid,
            // Exiting or mid-exec: its executable at least.
            command: command(pid).unwrap_or_else(|| String::from_utf8_lossy(&exe).into_owned()),
        })
        .collect();
    left.sort_by_key(|l| l.pid);
    left
}

/// SIGKILL everything running in `dir` (dev servers and their watchers) but `keep`,
/// their ancestors and, with `spare_ours`, what lazy-cow-tree runs: a dev server needs
/// no graceful shutdown, and left alive they'd write into the deleted worktree.
pub async fn kill_processes_in(dir: &Path, keep: &[i32], spare_ours: bool) {
    for _ in 0..3 {
        let pids = processes_in(dir, keep, spare_ours);
        if pids.is_empty() {
            return;
        }
        info!("killing {} process(es) in {}", pids.len(), dir.display());
        for (p, start, _) in &pids {
            // Not a process that got its pid since.
            if start_time(*p) == Some(*start) {
                unsafe { libc::kill(*p, libc::SIGKILL) };
            }
        }
        // Catch anything forked meanwhile.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
