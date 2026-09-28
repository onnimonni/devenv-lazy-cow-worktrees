//! One PostgreSQL cluster for every project and worktree, on the RAM disk. Disposable
//! dev/test data: fsync and friends are off. Every copy of a template runs
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

/// `name` in PATH, like a shell would find it (but without one).
pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Only the daemon is `postgres` without a password; checkouts log in as their own
/// roles with scram passwords, checked by this server through the proxy.
const HBA: &str = "local all postgres trust\nlocal all all scram-sha-256\n";

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl Postgres {
    /// The real server's unix socket (what the proxy connects to).
    pub fn socket(&self) -> PathBuf {
        self.dir.join(format!(".s.PGSQL.{}", self.port))
    }

    /// Start the cluster listening on `dir/.s.PGSQL.<port>` only.
    pub async fn start(
        dir: PathBuf,
        port: u16,
        ramdisk_mb: u64,
        bin: Option<PathBuf>,
        extra: Vec<(String, String)>,
    ) -> Result<Self> {
        let d = dir.clone();
        tokio::task::spawn_blocking(move || crate::ramdisk::ensure(&d, ramdisk_mb)).await??;
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
                .unwrap()
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
                    "--no-sync",
                    "--no-instructions",
                    "--encoding=UTF8",
                    "--locale=C",
                ])
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

        let mut settings = vec![
            // Unix socket only: clients come through localforest's proxy (pgproxy.rs).
            ("listen_addresses", String::new()),
            ("port", port.to_string()),
            ("unix_socket_directories", pg.dir.display().to_string()),
            ("fsync", "off".into()),
            ("synchronous_commit", "off".into()),
            ("full_page_writes", "off".into()),
            ("max_connections", "500".into()),
            // The default max_wal_size (1 GB) would crowd the RAM disk.
            ("min_wal_size", "32MB".into()),
            ("max_wal_size", "256MB".into()),
        ];
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
        // The project's (localforest.postgres.settings) come last and win.
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

    /// Create or update a checkout's login role. SUPERUSER: dev tooling expects it
    /// (CREATE EXTENSION, ecto.create, objects owned by the primary's role in cloned
    /// databases); which databases it can open is enforced by the proxy.
    pub async fn ensure_role(&self, role: &str, password: &str) -> Result<()> {
        let admin = self.admin().await?;
        let exists = admin
            .query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
            .await?
            .is_some();
        let verb = if exists { "ALTER" } else { "CREATE" };
        admin
            .batch_execute(&format!(
                "{verb} ROLE {} WITH LOGIN SUPERUSER PASSWORD '{}'",
                quote_ident(role),
                password.replace('\'', "''")
            ))
            .await
            .with_context(|| format!("creating role {role}"))?;
        Ok(())
    }

    pub async fn drop_role(&self, role: &str) -> Result<()> {
        let admin = self.admin().await?;
        let exists = admin
            .query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
            .await?
            .is_some();
        if exists {
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
    /// refuses to copy a database in use; clients reconnect).
    pub async fn snapshot(&self, src: &str, dst: &str) -> Result<()> {
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
