//! One PostgreSQL cluster for every project and worktree, on the RAM disk. Disposable
//! dev/test data: fsync and friends are off; a durable cluster
//! (`LAZY_COW_TREE_POSTGRES_DURABLE`) lives on disk with them on. Every copy of a template runs
//! `SET file_copy_method = clone` + `CREATE DATABASE ... STRATEGY FILE_COPY`
//! (PostgreSQL 18+), a copy-on-write clone of the files (clonefile on APFS,
//! copy_file_range reflinks on btrfs/XFS), so a worktree's database is ready in
//! milliseconds whatever its size (203 MB: 44 ms cloned, 458 ms copied).

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{process::Child, sync::Mutex};
use tracing::{debug, info, warn};

pub struct Postgres {
    /// Socket directory (the RAM disk).
    pub dir: PathBuf,
    pub port: u16,
    child: Mutex<Option<Child>>,
    /// Set in the session of every CREATE DATABASE from a template: `clone` unless
    /// `file_copy_method` is in the project's postgres settings.
    file_copy_method: String,
}

/// postgresql.conf settings lazy-cow-tree starts a cluster with (the project's come
/// after and win).
fn cluster_settings(
    dir: &std::path::Path,
    port: u16,
    durable: bool,
) -> Vec<(&'static str, String)> {
    let mut settings = vec![
        // Unix socket only: clients come through lazy-cow-tree's proxy (pgproxy.rs).
        ("listen_addresses", String::new()),
        ("port", port.to_string()),
        ("unix_socket_directories", dir.display().to_string()),
        ("max_connections", "1000".into()),
        // Compiling a query plan costs more than test suites' short queries save.
        ("jit", "off".into()),
    ];
    if !durable {
        settings.extend([
            ("fsync", "off".into()),
            ("synchronous_commit", "off".into()),
            ("full_page_writes", "off".into()),
            // As little WAL as PostgreSQL allows (it can't be off): crash recovery
            // only, none for rows of tables created or truncated in the same
            // transaction. No replication or point-in-time recovery to keep it for.
            ("wal_level", "minimal".into()),
            ("max_wal_senders", "0".into()),
            ("archive_mode", "off".into()),
            // The default max_wal_size (1 GB) would crowd the RAM disk; fewer
            // checkpoints during seeds and bulk loads.
            ("min_wal_size", "32MB".into()),
            ("max_wal_size", "256MB".into()),
            ("checkpoint_timeout", "30min".into()),
        ]);
    }
    settings
}

/// `name` in PATH, like a shell would find it (but without one).
pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Only the daemon is `postgres` without a password; checkouts log in as their own
/// roles with scram passwords, checked by this server through the proxy.
const HBA: &str = "local all postgres trust\nlocal all all scram-sha-256\n";

async fn role_exists(client: &tokio_postgres::Client, role: &str) -> Result<bool> {
    Ok(client
        .query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await?
        .is_some())
}

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl Postgres {
    /// The real server's unix socket (what the proxy connects to).
    pub fn socket(&self) -> PathBuf {
        self.dir.join(format!(".s.PGSQL.{}", self.port))
    }

    /// Start the cluster listening on `dir/.s.PGSQL.<port>` only: on the RAM disk
    /// with fsync, synchronous_commit and full_page_writes off, or `durable` in a
    /// plain directory with PostgreSQL's defaults.
    pub async fn start(
        dir: PathBuf,
        port: u16,
        ramdisk_mb: u64,
        durable: bool,
        bin: Option<PathBuf>,
        extra: Vec<(String, String)>,
    ) -> Result<Self> {
        let d = dir.clone();
        if durable {
            std::fs::create_dir_all(&d)?;
        } else {
            tokio::task::spawn_blocking(move || crate::ramdisk::ensure(&d, ramdisk_mb)).await??;
        }
        let file_copy_method = match extra.iter().find(|(k, _)| k == "file_copy_method") {
            None => "clone".to_string(),
            Some((_, v)) if v == "clone" || v == "copy" => v.clone(),
            Some((_, v)) => bail!("file_copy_method must be clone or copy, not {v:?}"),
        };
        let pg = Self {
            dir,
            port,
            child: Mutex::new(None),
            file_copy_method,
        };
        if let Ok(admin) = pg.connect("postgres").await {
            info!("using PostgreSQL already running in {}", pg.dir.display());
            std::fs::write(pg.dir.join("data/pg_hba.conf"), HBA)?;
            admin.batch_execute("SELECT pg_reload_conf()").await?;
            return Ok(pg);
        }

        let bin = match bin {
            Some(b) => b,
            None => which("postgres")
                .context("`postgres` not in PATH; add pkgs.postgresql_18 to devenv.nix packages")?
                .parent()
                .context("`postgres` in PATH has no parent directory")?
                .to_path_buf(),
        };
        let postgres = bin.join("postgres");
        let data = pg.dir.join("data");
        if !data.join("PG_VERSION").exists() {
            if data.exists() {
                std::fs::remove_dir_all(&data)?;
            }
            info!("initdb {}", data.display());
            let out = tokio::process::Command::new(bin.join("initdb"))
                .arg("-D")
                .arg(&data)
                .args([
                    "--username=postgres",
                    "--auth=trust",
                    "--no-instructions",
                    "--encoding=UTF8",
                    "--locale=C",
                ])
                .args((!durable).then_some("--no-sync"))
                .output()
                .await
                .context("running initdb")?;
            if !out.status.success() {
                bail!("initdb failed: {}", String::from_utf8_lossy(&out.stderr));
            }
        }
        std::fs::write(data.join("pg_hba.conf"), HBA)?;
        let version: u32 = std::fs::read_to_string(data.join("PG_VERSION"))?
            .trim()
            .parse()
            .unwrap_or(0);

        let mut settings = cluster_settings(&pg.dir, port, durable);
        if version >= 18 {
            // Also the server default, for CREATE DATABASE run by apps and tools.
            settings.push(("file_copy_method", pg.file_copy_method.clone()));
        } else {
            warn!("PostgreSQL {version}: no copy-on-write CREATE DATABASE (needs 18+)");
        }
        let log = std::fs::File::create(pg.dir.join("postgres.log"))?;
        let mut cmd = tokio::process::Command::new(&postgres);
        cmd.arg("-D").arg(&data);
        for (k, v) in &settings {
            cmd.arg("-c").arg(format!("{k}={v}"));
        }
        // The project's (lazy-cow-tree.postgres.settings) come last and win.
        for (k, v) in &extra {
            cmd.arg("-c").arg(format!("{k}={v}"));
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .context("starting postgres")?;
        *pg.child.lock().await = Some(child);

        for _ in 0..600 {
            if pg.connect("postgres").await.is_ok() {
                info!(
                    "PostgreSQL {version} on {}/.s.PGSQL.{port}",
                    pg.dir.display()
                );
                return Ok(pg);
            }
            if let Some(c) = pg.child.lock().await.as_mut()
                && let Some(status) = c.try_wait()?
            {
                bail!(
                    "postgres exited ({status}); see {}",
                    pg.dir.join("postgres.log").display()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("postgres did not start within 60 s")
    }

    /// Fast shutdown of the cluster this process started.
    pub async fn stop(&self) {
        if let Some(mut c) = self.child.lock().await.take() {
            if let Some(pid) = c.id() {
                unsafe { libc::kill(pid as i32, libc::SIGINT) };
            }
            let _ = tokio::time::timeout(Duration::from_secs(10), c.wait()).await;
        }
    }

    pub async fn connect(&self, db: &str) -> Result<tokio_postgres::Client> {
        let (client, conn) = tokio_postgres::Config::new()
            .host_path(&self.dir)
            .port(self.port)
            .user("postgres")
            .dbname(db)
            .connect_timeout(Duration::from_secs(2))
            .connect(tokio_postgres::NoTls)
            .await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(client)
    }

    async fn admin(&self) -> Result<tokio_postgres::Client> {
        self.connect("postgres").await
    }

    pub async fn databases(&self) -> Result<Vec<String>> {
        let rows = self
            .admin()
            .await?
            .query(
                "SELECT datname FROM pg_database WHERE datname NOT IN ('template0', 'template1')",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// Databases with their size in bytes, biggest first.
    pub async fn sizes(&self) -> Result<Vec<(String, i64)>> {
        let rows = self
            .admin()
            .await?
            .query(
                "SELECT datname::text, pg_database_size(oid) FROM pg_database \
                 WHERE datname NOT IN ('template0', 'template1') ORDER BY 2 DESC",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// (free, total) bytes of the filesystem the cluster is on (the RAM disk).
    pub fn disk(&self) -> Option<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(self.dir.as_os_str().as_bytes()).ok()?;
        // statfs, as df: macOS's statvfs has 32-bit counts.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let block = st.f_bsize as u64;
        Some((st.f_bavail as u64 * block, st.f_blocks as u64 * block))
    }

    /// Owner of `db`, None if it doesn't exist.
    pub async fn owner(&self, db: &str) -> Result<Option<String>> {
        Ok(self
            .admin()
            .await?
            .query_opt(
                "SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname = $1",
                &[&db],
            )
            .await?
            .map(|r| r.get(0)))
    }

    pub async fn exists(&self, db: &str) -> Result<bool> {
        Ok(self
            .admin()
            .await?
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&db])
            .await?
            .is_some())
    }

    /// The database's OID: a new one each time it's (re)created.
    pub async fn oid(&self, db: &str) -> Result<Option<u32>> {
        Ok(self
            .admin()
            .await?
            .query_opt("SELECT oid FROM pg_database WHERE datname = $1", &[&db])
            .await?
            .map(|r| r.get(0)))
    }

    pub async fn connections(&self, db: &str) -> Result<i64> {
        Ok(self
            .admin()
            .await?
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
                &[&db],
            )
            .await?
            .get(0))
    }

    pub async fn terminate(&self, db: &str) -> Result<()> {
        self.admin()
            .await?
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
                &[&db],
            )
            .await?;
        Ok(())
    }

    /// Create `db`, as a copy-on-write clone of `template` when given.
    pub async fn create(
        &self,
        db: &str,
        template: Option<&str>,
        owner: Option<&str>,
    ) -> Result<()> {
        let mut sql = format!("CREATE DATABASE {}", quote_ident(db));
        if let Some(t) = template {
            sql += &format!(" TEMPLATE {} STRATEGY FILE_COPY", quote_ident(t));
        }
        if let Some(o) = owner {
            sql += &format!(" OWNER {}", quote_ident(o));
        }
        let admin = self.admin().await?;
        // Copy-on-write clone of the template's files, whatever the server config
        // says. A separate statement: CREATE DATABASE refuses to share a query string
        // (implicit transaction). PostgreSQL < 18 doesn't know it: plain copy.
        if template.is_some()
            && let Err(e) = admin
                .batch_execute(&format!("SET file_copy_method = {}", self.file_copy_method))
                .await
        {
            debug!("no file_copy_method ({e}); copying");
        }
        admin
            .batch_execute(&sql)
            .await
            .with_context(|| format!("creating database {db}"))?;
        Ok(())
    }

    /// Create or update a checkout's login role: CREATEDB (`mix ecto.create` / `drop`
    /// of its own databases) and nothing more. Not a superuser, so it can't drop or
    /// alter other checkouts' databases and roles, or run programs (COPY TO PROGRAM).
    /// It owns its databases and, after `adopt`, everything in its cloned dev database;
    /// trusted extensions (pgcrypto, citext, ...) need only that. At most `connections`
    /// at once.
    pub async fn ensure_role(&self, role: &str, password: &str, connections: u32) -> Result<()> {
        self.upsert_role(
            role,
            &format!(
                "LOGIN NOSUPERUSER CREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS \
                 CONNECTION LIMIT {connections} PASSWORD '{}'",
                password.replace('\'', "''")
            ),
        )
        .await
    }

    /// The role objects in a template database belong to (named like the database):
    /// no login, no members, never owns a database, so REASSIGN OWNED from it (`adopt`)
    /// only moves objects.
    async fn ensure_owner_role(&self, role: &str) -> Result<()> {
        self.upsert_role(
            role,
            "NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT",
        )
        .await
    }

    async fn upsert_role(&self, role: &str, options: &str) -> Result<()> {
        let admin = self.admin().await?;
        let verb = if role_exists(&admin, role).await? {
            "ALTER"
        } else {
            "CREATE"
        };
        admin
            .batch_execute(&format!("{verb} ROLE {} WITH {options}", quote_ident(role)))
            .await
            .with_context(|| format!("creating role {role}"))?;
        Ok(())
    }

    /// In `db`, give everything `from` owns to `to` (tables, sequences, types,
    /// functions, schemas, extensions, default privileges), so `to` can migrate a
    /// cloned database. REASSIGN OWNED also moves the databases `from` owns, cluster
    /// wide: they are handed back in the same transaction, with CREATE DATABASE
    /// blocked meanwhile. Nothing to do when `from` doesn't exist.
    pub async fn adopt(&self, db: &str, from: &str, to: &str) -> Result<()> {
        if from == to || from == "postgres" {
            return Ok(());
        }
        let mut client = self.connect(db).await?;
        // The usual case, `from` owns nothing here (or doesn't exist): no locks at all.
        let owns = client
            .query_opt(
                "SELECT 1 FROM pg_shdepend d JOIN pg_roles r ON r.oid = d.refobjid
                 WHERE r.rolname = $1 AND d.deptype = 'o'
                   AND d.dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
                 LIMIT 1",
                &[&from],
            )
            .await?
            .is_some();
        if !owns {
            return Ok(());
        }
        let tx = client.transaction().await?;
        // Not stuck behind a client's open transaction for long. SHARE ROW EXCLUSIVE
        // conflicts with itself: concurrent adopts queue here instead of deadlocking.
        tx.batch_execute(
            "SET LOCAL lock_timeout = '10s';
             LOCK TABLE pg_catalog.pg_database IN SHARE ROW EXCLUSIVE MODE",
        )
        .await?;
        // Event triggers must belong to a superuser (made by checkout roles back when
        // they were ones): to postgres, not to `to`.
        let triggers: Vec<String> = tx
            .query(
                "SELECT evtname::text FROM pg_event_trigger WHERE evtowner = (SELECT oid FROM pg_roles WHERE rolname = $1)",
                &[&from],
            )
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        for t in &triggers {
            tx.batch_execute(&format!(
                "ALTER EVENT TRIGGER {} OWNER TO postgres",
                quote_ident(t)
            ))
            .await?;
        }
        let owned: Vec<String> = tx
            .query(
                "SELECT datname::text FROM pg_database WHERE datdba = (SELECT oid FROM pg_roles WHERE rolname = $1)",
                &[&from],
            )
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let mut sql = format!(
            "REASSIGN OWNED BY {} TO {};",
            quote_ident(from),
            quote_ident(to)
        );
        for d in &owned {
            sql += &format!(
                "ALTER DATABASE {} OWNER TO {};",
                quote_ident(d),
                quote_ident(from)
            );
        }
        tx.batch_execute(&sql)
            .await
            .with_context(|| format!("{db}: giving {from}'s objects to {to}"))?;
        tx.commit().await?;
        Ok(())
    }

    /// Every new database starts from template1: close `public` in it (and in
    /// `postgres`) to checkout roles (the default since PostgreSQL 15), since they may
    /// connect to both, and create `extensions` there as superuser, for extensions that
    /// aren't trusted (postgis, vector): databases made later have them, and apps'
    /// `CREATE EXTENSION IF NOT EXISTS` is a no-op.
    pub async fn prepare_templates(&self, extensions: &[String]) -> Result<()> {
        let admin = self.admin().await?;
        // Checkout roles were superusers before; nothing but the daemon's is now.
        let supers: Vec<String> = admin
            .query(
                "SELECT rolname::text FROM pg_roles WHERE rolsuper AND rolname <> 'postgres'",
                &[],
            )
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        for r in &supers {
            admin
                .batch_execute(&format!("ALTER ROLE {} NOSUPERUSER", quote_ident(r)))
                .await
                .with_context(|| format!("revoking SUPERUSER from {r}"))?;
            info!("{r}: no longer a superuser");
        }
        for db in ["template1", "postgres"] {
            self.connect(db)
                .await?
                .batch_execute("REVOKE CREATE ON SCHEMA public FROM PUBLIC")
                .await
                .with_context(|| format!("{db}: revoking CREATE on public"))?;
        }
        if let Err(e) = self.create_extensions("template1", extensions).await {
            warn!("{e:#}");
        }
        Ok(())
    }

    /// Make `role` the owner of database `db`.
    pub async fn set_owner(&self, db: &str, role: &str) -> Result<()> {
        self.admin()
            .await?
            .batch_execute(&format!(
                "ALTER DATABASE {} OWNER TO {}",
                quote_ident(db),
                quote_ident(role)
            ))
            .await
            .with_context(|| format!("giving database {db} to {role}"))?;
        Ok(())
    }

    /// `CREATE EXTENSION IF NOT EXISTS` each of `extensions` in `db`, as superuser.
    pub async fn create_extensions(&self, db: &str, extensions: &[String]) -> Result<()> {
        if extensions.is_empty() {
            return Ok(());
        }
        let client = self.connect(db).await?;
        for e in extensions {
            client
                .batch_execute(&format!(
                    "CREATE EXTENSION IF NOT EXISTS {} CASCADE",
                    quote_ident(e)
                ))
                .await
                .with_context(|| format!("{db}: CREATE EXTENSION {e}"))?;
        }
        Ok(())
    }

    pub async fn drop_role(&self, role: &str) -> Result<()> {
        let admin = self.admin().await?;
        if role_exists(&admin, role).await? {
            let r = quote_ident(role);
            admin
                .batch_execute(&format!(
                    "REASSIGN OWNED BY {r} TO postgres; DROP OWNED BY {r}; DROP ROLE {r}"
                ))
                .await
                .with_context(|| format!("dropping role {role}"))?;
        }
        Ok(())
    }

    /// Rename `old` to `new`, closing `old`'s connections first.
    pub async fn rename(&self, old: &str, new: &str) -> Result<()> {
        self.terminate(old).await?;
        self.admin()
            .await?
            .batch_execute(&format!(
                "ALTER DATABASE {} RENAME TO {}",
                quote_ident(old),
                quote_ident(new)
            ))
            .await
            .with_context(|| format!("renaming database {old} to {new}"))?;
        Ok(())
    }

    pub async fn role_exists(&self, role: &str) -> Result<bool> {
        Ok(self
            .admin()
            .await?
            .query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
            .await?
            .is_some())
    }

    /// `ALTER ROLE old RENAME TO new` (it keeps everything it owns and its grants;
    /// PostgreSQL clears its MD5 password, which `ensure_role` sets again).
    pub async fn rename_role(&self, old: &str, new: &str) -> Result<()> {
        self.admin()
            .await?
            .batch_execute(&format!(
                "ALTER ROLE {} RENAME TO {}",
                quote_ident(old),
                quote_ident(new)
            ))
            .await
            .with_context(|| format!("renaming role {old} to {new}"))?;
        Ok(())
    }

    /// Make `new` a member of `old`, so it may use and alter what `old` owns.
    pub async fn grant_role(&self, old: &str, new: &str) -> Result<()> {
        self.admin()
            .await?
            .batch_execute(&format!(
                "GRANT {} TO {}",
                quote_ident(old),
                quote_ident(new)
            ))
            .await
            .with_context(|| format!("granting {old} to {new}"))?;
        Ok(())
    }

    /// Hand everything `old` owns to `new` and drop `old` (no-op without `old`).
    pub async fn retire_role(&self, old: &str, new: &str) -> Result<()> {
        let admin = self.admin().await?;
        let exists = admin
            .query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&old])
            .await?
            .is_some();
        if !exists {
            return Ok(());
        }
        // REASSIGN / DROP OWNED act on the current database (and shared objects):
        // run them in each database the role owns objects in.
        let dbs = self.databases().await?;
        let (o, n) = (quote_ident(old), quote_ident(new));
        for db in dbs {
            let Ok(c) = self.connect(&db).await else {
                continue;
            };
            c.batch_execute(&format!("REASSIGN OWNED BY {o} TO {n}; DROP OWNED BY {o}"))
                .await
                .with_context(|| format!("moving {old}'s objects in {db} to {new}"))?;
        }
        admin
            .batch_execute(&format!("DROP ROLE {o}"))
            .await
            .with_context(|| format!("dropping role {old}"))?;
        Ok(())
    }

    pub async fn drop(&self, db: &str) -> Result<()> {
        self.admin()
            .await?
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                quote_ident(db)
            ))
            .await
            .with_context(|| format!("dropping database {db}"))?;
        Ok(())
    }

    /// Replace `dst` with a clone of `src`, closing `src`'s connections (PostgreSQL
    /// refuses to copy a database in use; clients reconnect). The clone is built as
    /// `<dst>_next`, its objects given to the NOLOGIN role named `dst`
    /// (`ensure_owner_role`), not `src`'s owner: clones of `dst` hand them to their
    /// checkout's role (`adopt`). Only then are the old `dst` dropped and the new one
    /// renamed in, holding `lock` (the one clones of `dst` are made under), so a clone
    /// never finds `dst` missing.
    pub async fn snapshot(&self, src: &str, dst: &str, lock: &Mutex<()>) -> Result<()> {
        let tmp = format!("{dst}_next");
        self.drop(&tmp).await?;
        let mut last = None;
        for _ in 0..20 {
            self.terminate(src).await?;
            match self.create(&tmp, Some(src), None).await {
                Ok(()) => {
                    last = None;
                    break;
                }
                // A pool reconnected in between: try again.
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if let Some(e) = last {
            return Err(e);
        }
        if let Some(from) = self.owner(src).await? {
            self.ensure_owner_role(dst).await?;
            self.adopt(&tmp, &from, dst).await?;
        }
        let _g = lock.lock().await;
        self.drop(dst).await?;
        self.admin()
            .await?
            .batch_execute(&format!(
                "ALTER DATABASE {} RENAME TO {}",
                quote_ident(&tmp),
                quote_ident(dst)
            ))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway cluster from `initdb` / `postgres` in PATH (skipped without them,
    /// e.g. CI's unit test job; the e2e job covers the daemon).
    async fn cluster() -> Option<(Postgres, tempfile::TempDir)> {
        let bin = which("initdb")?.parent()?.to_path_buf();
        // Short: unix socket paths are limited to ~104 bytes.
        let tmp = tempfile::Builder::new()
            .prefix("lfpg")
            .tempdir_in("/tmp")
            .unwrap();
        let data = tmp.path().join("data");
        let out = std::process::Command::new(bin.join("initdb"))
            .arg("-D")
            .arg(&data)
            .args(["--username=postgres", "--auth=trust", "--no-sync"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let child = tokio::process::Command::new(bin.join("postgres"))
            .arg("-D")
            .arg(&data)
            // The port `connect` uses, not PGPORT's (set in a lazy-cow-tree shell).
            .args([
                "-c",
                "port=5432",
                "-c",
                "listen_addresses=",
                "-c",
                "fsync=off",
                "-c",
            ])
            .arg(format!("unix_socket_directories={}", tmp.path().display()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pg = Postgres {
            dir: tmp.path().to_path_buf(),
            port: 5432,
            child: Mutex::new(Some(child)),
            file_copy_method: "copy".into(),
        };
        for _ in 0..100 {
            if pg.connect("postgres").await.is_ok() {
                return Some((pg, tmp));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("test cluster did not start");
    }

    async fn as_role(pg: &Postgres, role: &str, db: &str) -> tokio_postgres::Client {
        let (client, conn) = tokio_postgres::Config::new()
            .host_path(&pg.dir)
            .port(pg.port)
            .user(role)
            .dbname(db)
            .connect(tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        client
    }

    async fn owners(pg: &Postgres, db: &str) -> Vec<String> {
        pg.connect(db)
            .await
            .unwrap()
            .query(
                "SELECT DISTINCT pg_get_userbyid(relowner)::text FROM pg_class WHERE relnamespace = 'public'::regnamespace
                 UNION SELECT pg_get_userbyid(typowner)::text FROM pg_type WHERE typnamespace = 'public'::regnamespace
                 UNION SELECT pg_get_userbyid(extowner)::text FROM pg_extension WHERE extname <> 'plpgsql'",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect()
    }

    #[tokio::test]
    async fn checkout_roles_are_no_superusers() {
        let Some((pg, _tmp)) = cluster().await else {
            eprintln!("no initdb in PATH; skipped");
            return;
        };
        // From before: a superuser checkout role (another naming scheme) with a
        // database, tables and an event trigger in it.
        let admin = pg.connect("postgres").await.unwrap();
        admin
            .batch_execute("CREATE ROLE old LOGIN SUPERUSER")
            .await
            .unwrap();
        pg.create("app_old_x", None, Some("old")).await.unwrap();
        as_role(&pg, "old", "app_old_x")
            .await
            .batch_execute(
                "CREATE TABLE t(id int); CREATE TABLE u(id int);
                 CREATE FUNCTION evt() RETURNS event_trigger LANGUAGE plpgsql AS 'BEGIN END';
                 CREATE EVENT TRIGGER et ON ddl_command_start EXECUTE FUNCTION evt()",
            )
            .await
            .unwrap();
        pg.prepare_templates(&[]).await.unwrap();
        let supers = admin
            .query(
                "SELECT 1 FROM pg_roles WHERE rolsuper AND rolname <> 'postgres'",
                &[],
            )
            .await
            .unwrap();
        assert!(supers.is_empty(), "superusers left");
        for role in ["app", "app-x", "app-y"] {
            pg.ensure_role(role, "pw", 200).await.unwrap();
        }
        pg.create("app_dev", None, Some("app")).await.unwrap();
        pg.create("app_test", None, Some("app")).await.unwrap();
        // The primary's migrations, with a trusted extension.
        as_role(&pg, "app", "app_dev")
            .await
            .batch_execute(
                "CREATE EXTENSION pgcrypto; CREATE TYPE mood AS ENUM ('ok');
                 CREATE TABLE seeds(id serial PRIMARY KEY, m mood); INSERT INTO seeds(m) VALUES ('ok')",
            )
            .await
            .unwrap();

        pg.snapshot("app_dev", "app_template", &Mutex::new(()))
            .await
            .unwrap();
        assert_eq!(owners(&pg, "app_template").await, ["app_template"]);
        // REASSIGN OWNED moved the primary's databases too; they were handed back.
        assert_eq!(pg.owner("app_dev").await.unwrap().as_deref(), Some("app"));
        assert_eq!(pg.owner("app_test").await.unwrap().as_deref(), Some("app"));

        // A worktree's clone: its role can migrate what came from the template.
        pg.create("app_dev_x", Some("app_template"), Some("app-x"))
            .await
            .unwrap();
        pg.adopt("app_dev_x", "app_template", "app-x")
            .await
            .unwrap();
        pg.adopt("app_dev_x", "app", "app-x").await.unwrap();
        assert_eq!(owners(&pg, "app_dev_x").await, ["app-x"]);
        assert_eq!(pg.owner("app_dev").await.unwrap().as_deref(), Some("app"));
        let x = as_role(&pg, "app-x", "app_dev_x").await;
        x.batch_execute(
            "ALTER TABLE seeds ADD COLUMN y int; ALTER TYPE mood ADD VALUE 'meh';
             DROP EXTENSION pgcrypto; CREATE EXTENSION citext",
        )
        .await
        .unwrap();

        // Its own databases: yes. Anything else: no.
        let x = as_role(&pg, "app-x", "postgres").await;
        x.batch_execute("CREATE DATABASE app_test_x").await.unwrap();
        x.batch_execute("DROP DATABASE app_test_x").await.unwrap();
        for sql in [
            "DROP DATABASE app_dev",
            "DROP DATABASE app_template",
            "ALTER DATABASE app_dev OWNER TO \"app-x\"",
            "ALTER ROLE app SUPERUSER",
            "ALTER ROLE \"app-y\" PASSWORD 'mine'",
            "DROP ROLE \"app-y\"",
            "SET ROLE app_template",
            "COPY (SELECT 1) TO PROGRAM 'true'",
            "SELECT pg_read_file('postgresql.conf')",
            "CREATE TABLE evil(x int)",
        ] {
            assert!(x.batch_execute(sql).await.is_err(), "{sql} went through");
        }
        let t1 = as_role(&pg, "app-x", "template1").await;
        assert!(t1.batch_execute("CREATE TABLE evil(x int)").await.is_err());
        // The old role's database goes to the checkout's role, event trigger to postgres.
        pg.set_owner("app_old_x", "app-x").await.unwrap();
        pg.adopt("app_old_x", "old", "app-x").await.unwrap();
        assert_eq!(owners(&pg, "app_old_x").await, ["app-x"]);
        let evt: String = pg
            .connect("app_old_x")
            .await
            .unwrap()
            .query_one(
                "SELECT pg_get_userbyid(evtowner)::text FROM pg_event_trigger",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(evt, "postgres");
        as_role(&pg, "app-x", "app_old_x")
            .await
            .batch_execute("ALTER TABLE t ADD COLUMN y int; DROP TABLE u")
            .await
            .unwrap();
        // Nothing left to hand over: no-op.
        pg.adopt("app_old_x", "old", "app-x").await.unwrap();

        // Concurrent adopts queue on pg_database instead of deadlocking.
        for (db, role) in [("app_dev_y", "app-y"), ("app_dev_z", "app-z")] {
            pg.ensure_role(role, "pw", 200).await.unwrap();
            pg.create(db, Some("app_template"), Some(role))
                .await
                .unwrap();
        }
        let (a, b) = tokio::join!(
            pg.adopt("app_dev_y", "app_template", "app-y"),
            pg.adopt("app_dev_z", "app_template", "app-z"),
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(owners(&pg, "app_dev_y").await, ["app-y"]);
        assert_eq!(owners(&pg, "app_dev_z").await, ["app-z"]);
        pg.stop().await;
    }
}
