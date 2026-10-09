//! Test slots: at most N test suites (`mix test`, `cargo test`, ...) run at once
//! across every checkout, so parallel agents don't thrash the CPU into timeouts that
//! look like flaky tests. The devenv module's test command wrappers (bash) open the
//! N slot files on fds and run `lazy-cow-tree slot acquire`, which `flock`s one of
//! those inherited fds: the lock belongs to the open file, so it stays with the
//! wrapper, which closes the others and `exec`s the real command. The slot is free
//! again when that command (and whatever kept the fd) exits.

use std::{io::Write, os::fd::FromRawFd, path::PathBuf, time::Duration};

use anyhow::{Result, bail};

use crate::config;

/// `LAZY_COW_TREE_TEST_SLOTS`, else a quarter of the CPUs, at least 2; 0 turns slots
/// off.
pub fn count() -> usize {
    if let Some(n) = std::env::var("LAZY_COW_TREE_TEST_SLOTS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
    {
        return std::cmp::min(n, 50);
    }
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cpus / 4).clamp(2, 50)
}

/// Where the slot files are (created).
pub fn dir() -> Result<PathBuf> {
    let d = config::home().join("slots");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Lock one of `fds` (the slot files, opened by the caller), waiting for one to come
/// free, and write who holds it (`what`) into it. Ok(the fd locked).
pub fn acquire(fds: &[i32], what: &str) -> Result<i32> {
    if fds.is_empty() {
        bail!("no slot fds");
    }
    let mut told = false;
    loop {
        for &fd in fds {
            if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                // Who holds it, for those waiting.
                let mut f = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
                let _ = f.set_len(0);
                let _ = std::io::Seek::rewind(&mut *f);
                let _ = writeln!(f, "{what}");
                if told {
                    eprintln!("lazy-cow-tree: got a test slot");
                }
                return Ok(fd);
            }
        }
        if !told {
            told = true;
            let holders: Vec<String> = fds
                .iter()
                .filter_map(|&fd| {
                    let mut s = String::new();
                    let mut f =
                        std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
                    std::io::Seek::rewind(&mut *f).ok()?;
                    std::io::Read::read_to_string(&mut *f, &mut s).ok()?;
                    let s = s.trim().to_string();
                    (!s.is_empty()).then_some(s)
                })
                .collect();
            eprintln!(
                "lazy-cow-tree: waiting for a test slot ({} run at once: {}); \
                 LAZY_COW_TREE_TEST_SLOTS changes how many",
                fds.len(),
                holders.join("; ")
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
