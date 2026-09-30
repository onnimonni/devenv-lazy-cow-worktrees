//! Where the daemon keeps its state (`LAZY_COW_TREE_HOME`): sockets, PostgreSQL's
//! directories, the local CA and the per-machine secret.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Unix seconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("LAZY_COW_TREE_HOME") {
        return PathBuf::from(h);
    }
    // Tests never see (or move) the user's real state.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("lazy-cow-tree-test-{}", std::process::id()));
    }
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        state_dir(&home.join(".local/state"))
    })
    .clone()
}

/// `<state>/lazy-cow-tree`, moved there from the former `<state>/localforest` (CA,
/// secret, databases) the first time; where moving fails (a RAM disk mounted inside,
/// say) the old one stays in use.
pub(crate) fn state_dir(state: &Path) -> PathBuf {
    let new = state.join("lazy-cow-tree");
    let old = state.join("localforest");
    if new.exists() || !old.exists() {
        return new;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => {
            tracing::info!("moved {} to {}", old.display(), new.display());
            new
        }
        Err(e) => {
            tracing::warn!("keeping state in {} (moving it failed: {e})", old.display());
            old
        }
    }
}

pub fn socket_path() -> PathBuf {
    home().join("lazy-cow-tree.sock")
}

/// RAM disk mount point; also PostgreSQL's unix socket directory.
pub fn pg_dir() -> PathBuf {
    home().join("pg")
}

/// Data and socket directory of a durable cluster (`Global::postgres_durable`).
pub fn pg_durable_dir() -> PathBuf {
    home().join("pg-durable")
}

pub fn ca_cert_path() -> PathBuf {
    home().join("ca/ca.pem")
}

/// Per-machine random secret that checkout passwords derive from, created on first
/// use (0600), so the shell hook and the daemon agree without talking.
pub fn secret() -> Result<Vec<u8>> {
    secret_in(&home())
}

pub(crate) fn secret_in(dir: &Path) -> Result<Vec<u8>> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join("secret");
    if let Ok(s) = std::fs::read(&path)
        && s.len() >= 32
    {
        return Ok(s);
    }
    std::fs::create_dir_all(dir)?;
    let mut s = vec![0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut s))
        .context("reading /dev/urandom")?;
    // Written in full, then linked into place: a racing reader never sees it partial.
    let tmp = dir.join(format!("secret.{}.tmp", hex::encode(&s[..8])));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?
        .write_all(&s)?;
    let linked = std::fs::hard_link(&tmp, &path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(s),
        // Another process won the race.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(std::fs::read(&path)?),
        Err(e) => Err(e.into()),
    }
}
