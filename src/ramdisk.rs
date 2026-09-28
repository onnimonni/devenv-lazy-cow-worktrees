//! APFS RAM disk for PostgreSQL on macOS. APFS (not HFS+) because PostgreSQL 18's
//! `file_copy_method = clone` needs clonefile(2) for copy-on-write CREATE DATABASE.
//!
//! macOS has no public API to create a RAM disk or an APFS container, so this runs
//! hdiutil and diskutil directly (no shell). Elsewhere it's a plain directory.

use std::path::Path;

use anyhow::Result;

#[cfg(target_os = "macos")]
pub fn ensure(mount: &Path, size_mb: u64) -> Result<()> {
    use anyhow::{Context, bail};
    use std::process::Command;

    std::fs::create_dir_all(mount)?;
    if let Some(mb) = mounted_mb(mount)? {
        // APFS keeps a little for itself: only a real mismatch is worth a word.
        if mb.abs_diff(size_mb) > size_mb / 10 {
            tracing::warn!(
                "the RAM disk at {} has {mb} MB, not the configured {size_mb} MB; `localforest down --eject` and a restart resize it (emptying every database)",
                mount.display()
            );
        }
        return Ok(());
    }
    if std::fs::read_dir(mount)?.next().is_some() {
        bail!(
            "{} is not empty; move its contents away so the RAM disk can mount there",
            mount.display()
        );
    }
    let run = |cmd: &str, args: &[&str]| -> Result<String> {
        let out = Command::new(cmd)
            .args(args)
            .output()
            .with_context(|| format!("running {cmd}"))?;
        if !out.status.success() {
            bail!(
                "{cmd} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let disk_from = |out: &str| -> Result<String> {
        out.lines()
            .find_map(|l| l.strip_prefix("Disk from APFS operation:"))
            .map(|d| d.trim().to_string())
            .context("unexpected diskutil output")
    };

    // ram://<512-byte sectors>
    let sectors = format!("ram://{}", size_mb * 2048);
    let dev = run(
        "/usr/bin/hdiutil",
        &["attach", "-nobrowse", "-nomount", &sectors],
    )?
    .trim()
    .to_string();
    let result = (|| -> Result<()> {
        let container = disk_from(&run(
            "/usr/sbin/diskutil",
            &["apfs", "createContainer", &dev],
        )?)?;
        let volume = disk_from(&run(
            "/usr/sbin/diskutil",
            &[
                "apfs",
                "addVolume",
                &container,
                "APFS",
                "localforest-pg",
                "-nomount",
            ],
        )?)?;
        run(
            "/usr/sbin/diskutil",
            &[
                "mount",
                "nobrowse",
                "-mountPoint",
                &mount.to_string_lossy(),
                &volume,
            ],
        )?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = run("/usr/bin/hdiutil", &["detach", &dev, "-force"]);
        return Err(e);
    }
    std::fs::set_permissions(mount, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    tracing::info!("mounted {size_mb} MB APFS RAM disk at {}", mount.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_mounted(mount: &Path) -> Result<bool> {
    Ok(mounted_mb(mount)?.is_some())
}

/// Size in MB of the filesystem mounted at `mount`, None when nothing is.
#[cfg(target_os = "macos")]
fn mounted_mb(mount: &Path) -> Result<Option<u64>> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    let canonical = mount.canonicalize()?;
    let c = CString::new(canonical.as_os_str().as_bytes())?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let on = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) };
    if on.to_bytes() != canonical.as_os_str().as_bytes() {
        return Ok(None);
    }
    Ok(Some(st.f_blocks * u64::from(st.f_bsize) / (1024 * 1024)))
}

#[cfg(target_os = "macos")]
pub fn eject(mount: &Path) -> Result<()> {
    if is_mounted(mount)? {
        let out = std::process::Command::new("/usr/bin/hdiutil")
            .args(["detach", &mount.to_string_lossy(), "-force"])
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "hdiutil detach failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn ensure(mount: &Path, _size_mb: u64) -> Result<()> {
    std::fs::create_dir_all(mount)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn eject(_mount: &Path) -> Result<()> {
    Ok(())
}
