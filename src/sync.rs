//! Git side: fetch, then bring the main checkout and every worktree up to date.
//! Merges are computed in memory first so a conflict never touches a worktree.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{
    AutotagOption, BranchType, Cred, CredentialType, FetchOptions, FetchPrune, Oid,
    RemoteCallbacks, Repository, RepositoryState, Status, StatusOptions, build::CheckoutBuilder,
};
use tracing::{debug, info, warn};

pub struct Syncer {
    /// Main checkout (not a linked worktree).
    pub path: PathBuf,
    pub remote: String,
    pub base: String,
    /// Used for https remotes on `token_host`.
    pub token: Option<String>,
    pub token_host: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    FastForward,
    Merged(Oid),
    Conflict,
}

impl Syncer {
    /// Default branch from `refs/remotes/<remote>/HEAD`, falling back to `main`.
    pub fn default_base(path: &Path, remote: &str) -> String {
        Repository::open(path)
            .ok()
            .and_then(|r| {
                let head = r
                    .find_reference(&format!("refs/remotes/{remote}/HEAD"))
                    .ok()?;
                let target = head.symbolic_target().ok()??.to_string();
                target
                    .strip_prefix(&format!("refs/remotes/{remote}/"))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "main".to_string())
    }

    /// Fetch and update every checkout; returns the work dirs whose HEAD moved.
    pub fn sync(&self) -> Result<Vec<PathBuf>> {
        let repo = Repository::open(&self.path)?;
        self.fetch(&repo)?;

        let mut checked_out = Vec::new();
        let mut checkouts = vec![("main checkout".to_string(), Repository::open(&self.path)?)];
        for name in repo.worktrees()?.iter().filter_map(|n| n.ok().flatten()) {
            let wt = repo.find_worktree(name)?;
            if wt.validate().is_err() {
                debug!("skipping missing worktree {name}");
                continue;
            }
            checkouts.push((
                format!("worktree {name}"),
                Repository::open_from_worktree(&wt)?,
            ));
        }

        let mut moved = Vec::new();
        for (label, co) in &checkouts {
            if let Ok(head) = co.head()
                && let Ok(b) = head.shorthand()
                && head.is_branch()
            {
                checked_out.push(b.to_string());
            }
            let before = co.head().ok().and_then(|h| h.target());
            if let Err(e) = self.sync_checkout(co, label) {
                warn!("{label}: {e:#}");
            }
            if co.head().ok().and_then(|h| h.target()) != before
                && let Some(w) = co.workdir()
            {
                moved.push(w.to_path_buf());
            }
        }

        if !checked_out.contains(&self.base)
            && let Err(e) = self.fast_forward_ref(&repo)
        {
            warn!("updating {}: {e:#}", self.base);
        }
        Ok(moved)
    }

    pub fn fetch(&self, repo: &Repository) -> Result<()> {
        self.fetch_refspecs(repo, &[])
    }

    /// Fetch `refspecs` from the remote (none: its configured ones).
    pub fn fetch_refspecs(&self, repo: &Repository, refspecs: &[String]) -> Result<()> {
        let mut remote = repo
            .find_remote(&self.remote)
            .with_context(|| format!("no remote named {}", self.remote))?;
        let mut cb = RemoteCallbacks::new();
        let mut tries = 0;
        cb.credentials(|url, user, allowed| {
            tries += 1;
            credentials(
                url,
                user,
                allowed,
                tries,
                self.token.as_deref(),
                &self.token_host,
            )
        });
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(cb)
            .prune(FetchPrune::On)
            .download_tags(AutotagOption::Auto);
        remote
            .fetch(refspecs, Some(&mut fo), Some("localforest: fetch"))
            .with_context(|| format!("fetching {}", self.remote))?;
        debug!("fetched {}", self.remote);
        Ok(())
    }

    fn sync_checkout(&self, repo: &Repository, label: &str) -> Result<()> {
        if repo.state() != RepositoryState::Clean {
            info!("{label}: {:?} in progress, skipping", repo.state());
            return Ok(());
        }
        let head = repo.head()?;
        if !head.is_branch() {
            debug!("{label}: detached HEAD, skipping");
            return Ok(());
        }
        let branch = head.shorthand().unwrap_or_default().to_string();
        if is_dirty(repo)? {
            info!("{label} ({branch}): uncommitted changes, skipping");
            return Ok(());
        }

        let local = repo.find_branch(&branch, BranchType::Local)?;
        let upstream = local
            .upstream()
            .ok()
            .and_then(|u| u.get().name().ok().map(str::to_string))
            .unwrap_or_else(|| format!("refs/remotes/{}/{branch}", self.remote));
        if repo.find_reference(&upstream).is_ok() {
            self.report(label, &branch, &upstream, merge_into(repo, &upstream)?);
        }

        if branch != self.base {
            let base = format!("refs/remotes/{}/{}", self.remote, self.base);
            self.report(label, &branch, &base, merge_into(repo, &base)?);
        }
        Ok(())
    }

    fn report(&self, label: &str, branch: &str, from: &str, outcome: Outcome) {
        let from = from.strip_prefix("refs/remotes/").unwrap_or(from);
        match outcome {
            Outcome::UpToDate => debug!("{label} ({branch}): up to date with {from}"),
            Outcome::FastForward => info!("{label} ({branch}): fast-forwarded to {from}"),
            Outcome::Merged(id) => {
                info!("{label} ({branch}): merged {from} ({:.7})", id.to_string())
            }
            Outcome::Conflict => {
                warn!("{label} ({branch}): merging {from} conflicts, left untouched")
            }
        }
    }

    /// Base branch isn't checked out anywhere: just move the ref if it's a fast-forward.
    fn fast_forward_ref(&self, repo: &Repository) -> Result<()> {
        let Ok(mut local) = repo.find_reference(&format!("refs/heads/{}", self.base)) else {
            return Ok(());
        };
        let theirs = repo
            .find_reference(&format!("refs/remotes/{}/{}", self.remote, self.base))?
            .peel_to_commit()?
            .id();
        let ours = local.peel_to_commit()?.id();
        if ours != theirs && repo.graph_descendant_of(theirs, ours)? {
            local.set_target(theirs, "localforest: fast-forward")?;
            info!("{}: fast-forwarded (not checked out)", self.base);
        }
        Ok(())
    }
}

fn credentials(
    url: &str,
    user: Option<&str>,
    allowed: CredentialType,
    tries: u32,
    token: Option<&str>,
    token_host: &str,
) -> Result<Cred, git2::Error> {
    // libgit2 retries forever on bad credentials.
    if tries > 4 {
        return Err(git2::Error::from_str("authentication failed"));
    }
    let user = user.unwrap_or("git");
    if allowed.contains(CredentialType::SSH_KEY) {
        if tries == 1 {
            return Cred::ssh_key_from_agent(user);
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let key = ["id_ed25519", "id_ecdsa", "id_rsa"]
            .iter()
            .map(|k| home.join(".ssh").join(k))
            .nth((tries - 2) as usize)
            .filter(|p| p.exists());
        if let Some(key) = key {
            return Cred::ssh_key(user, None, &key, None);
        }
    }
    if allowed.contains(CredentialType::USER_PASS_PLAINTEXT)
        && let Some(t) = token
        && remote_host(url) == Some(token_host)
    {
        return Cred::userpass_plaintext("x-access-token", t);
    }
    if allowed.contains(CredentialType::DEFAULT) {
        return Cred::default();
    }
    Err(git2::Error::from_str("no usable credentials"))
}

fn remote_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host.split(':').next()
}

fn is_dirty(repo: &Repository) -> Result<bool> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(false)
        .include_ignored(false)
        .exclude_submodules(true);
    Ok(repo
        .statuses(Some(&mut opts))?
        .iter()
        .any(|e| e.status() != Status::CURRENT))
}

/// Merge `their_ref` into the checked-out branch of `repo`.
pub fn merge_into(repo: &Repository, their_ref: &str) -> Result<Outcome> {
    let theirs = repo.find_reference(their_ref)?.peel_to_commit()?;
    let annotated = repo.find_annotated_commit(theirs.id())?;
    let (analysis, _) = repo.merge_analysis(&[&annotated])?;
    if analysis.is_up_to_date() {
        return Ok(Outcome::UpToDate);
    }

    let head = repo.head()?;
    let head_name = head.name()?.to_string();
    let ours = head.peel_to_commit()?;
    let mut checkout = CheckoutBuilder::new();
    checkout.safe();

    if analysis.is_fast_forward() {
        repo.checkout_tree(theirs.as_object(), Some(&mut checkout))?;
        repo.find_reference(&head_name)?.set_target(
            theirs.id(),
            &format!("localforest: fast-forward to {their_ref}"),
        )?;
        return Ok(Outcome::FastForward);
    }

    let mut index = repo.merge_commits(&ours, &theirs, None)?;
    if index.has_conflicts() {
        return Ok(Outcome::Conflict);
    }
    let tree = repo.find_tree(index.write_tree_to(repo)?)?;
    repo.checkout_tree(tree.as_object(), Some(&mut checkout))?;
    let sig = repo.signature()?;
    let branch = head_name.strip_prefix("refs/heads/").unwrap_or(&head_name);
    let from = their_ref.strip_prefix("refs/remotes/").unwrap_or(their_ref);
    let msg = format!("Merge remote-tracking branch '{from}' into {branch}");
    let id = repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &[&ours, &theirs])?;
    Ok(Outcome::Merged(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Signature, WorktreeAddOptions};
    use std::fs;
    use tempfile::TempDir;

    fn sig() -> Signature<'static> {
        Signature::now("Test", "test@example.com").unwrap()
    }

    /// Commit `files` on top of `refname` (creating it if missing) without a checkout.
    fn commit_to(repo: &Repository, refname: &str, files: &[(&str, &str)], msg: &str) -> Oid {
        let parent = repo
            .find_reference(refname)
            .ok()
            .map(|r| r.peel_to_commit().unwrap());
        let mut tb = repo
            .treebuilder(parent.as_ref().map(|p| p.tree().unwrap()).as_ref())
            .unwrap();
        for (name, content) in files {
            tb.insert(name, repo.blob(content.as_bytes()).unwrap(), 0o100644)
                .unwrap();
        }
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let parents: Vec<_> = parent.iter().collect();
        repo.commit(Some(refname), &sig(), &sig(), msg, &tree, &parents)
            .unwrap()
    }

    struct Fixture {
        _dir: TempDir,
        origin: Repository,
        main: PathBuf,
        wt: PathBuf,
    }

    /// origin (bare) with main + feat; clone with main checked out and a `feat` worktree.
    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let origin = Repository::init_bare(dir.path().join("origin.git")).unwrap();
        commit_to(&origin, "refs/heads/main", &[("a.txt", "a\n")], "init");
        commit_to(&origin, "refs/heads/feat", &[("f.txt", "f\n")], "feat");
        origin.set_head("refs/heads/main").unwrap();

        let main = dir.path().join("clone");
        let repo = Repository::clone(origin.path().to_str().unwrap(), &main).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();

        let feat = repo
            .find_reference("refs/remotes/origin/feat")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let mut b = repo.branch("feat", &feat, false).unwrap();
        b.set_upstream(Some("origin/feat")).unwrap();
        let wt = dir.path().join("wt-feat");
        let r = repo.find_reference("refs/heads/feat").unwrap();
        repo.worktree(
            "feat",
            &wt,
            Some(WorktreeAddOptions::new().reference(Some(&r))),
        )
        .unwrap();
        Fixture {
            _dir: dir,
            origin,
            main,
            wt,
        }
    }

    fn syncer(f: &Fixture) -> Syncer {
        Syncer {
            path: f.main.clone(),
            remote: "origin".into(),
            base: "main".into(),
            token: None,
            token_host: "github.com".into(),
        }
    }

    fn head(path: &Path) -> git2::Commit<'static> {
        let repo = Box::leak(Box::new(Repository::open(path).unwrap()));
        repo.head().unwrap().peel_to_commit().unwrap()
    }

    #[test]
    fn pulls_main_and_merges_into_worktrees() {
        let f = fixture();
        let new_main = commit_to(&f.origin, "refs/heads/main", &[("b.txt", "b\n")], "main 2");
        let new_feat = commit_to(&f.origin, "refs/heads/feat", &[("g.txt", "g\n")], "feat 2");

        let moved = syncer(&f).sync().unwrap();
        assert_eq!(moved.len(), 2, "main checkout and worktree moved");

        assert_eq!(head(&f.main).id(), new_main);
        assert_eq!(fs::read_to_string(f.main.join("b.txt")).unwrap(), "b\n");

        let wt_head = head(&f.wt);
        assert_eq!(wt_head.parent_count(), 2);
        assert_eq!(wt_head.parent_id(0).unwrap(), new_feat);
        assert_eq!(wt_head.parent_id(1).unwrap(), new_main);
        for file in ["a.txt", "b.txt", "f.txt", "g.txt"] {
            assert!(f.wt.join(file).exists(), "{file} missing in worktree");
        }
        let wt_repo = Repository::open(&f.wt).unwrap();
        assert!(!is_dirty(&wt_repo).unwrap());

        // Idempotent.
        assert!(syncer(&f).sync().unwrap().is_empty());
        assert_eq!(head(&f.wt).id(), wt_head.id());
    }

    #[test]
    fn leaves_conflicting_worktree_untouched() {
        let f = fixture();
        commit_to(
            &f.origin,
            "refs/heads/main",
            &[("f.txt", "main version\n")],
            "clash",
        );
        let before = head(&f.wt).id();

        syncer(&f).sync().unwrap();

        assert_eq!(head(&f.wt).id(), before);
        assert_eq!(fs::read_to_string(f.wt.join("f.txt")).unwrap(), "f\n");
        assert_eq!(
            Repository::open(&f.wt).unwrap().state(),
            RepositoryState::Clean
        );
    }

    #[test]
    fn skips_dirty_worktree() {
        let f = fixture();
        commit_to(&f.origin, "refs/heads/main", &[("b.txt", "b\n")], "main 2");
        fs::write(f.wt.join("f.txt"), "local edit\n").unwrap();
        let before = head(&f.wt).id();

        syncer(&f).sync().unwrap();

        assert_eq!(head(&f.wt).id(), before);
        assert_eq!(
            fs::read_to_string(f.wt.join("f.txt")).unwrap(),
            "local edit\n"
        );
    }

    #[test]
    fn resolves_main_checkout_from_worktree() {
        let f = fixture();
        let got = crate::config::primary_root(&f.wt).unwrap();
        assert_eq!(got, f.main.canonicalize().unwrap());
        let (root, wt, _) = crate::config::locate(&f.wt).unwrap();
        assert_eq!(root, got);
        // Named after its git admin dir (`feat`), unique in the repository.
        assert_eq!(wt.as_deref(), Some("feat"));
        assert_eq!(Syncer::default_base(&f.main, "origin"), "main");
    }

    #[test]
    fn extracts_remote_host() {
        assert_eq!(
            remote_host("https://github.com/o/r.git"),
            Some("github.com")
        );
        assert_eq!(
            remote_host("https://u:p@ghe.corp:8443/o/r"),
            Some("ghe.corp")
        );
        assert_eq!(remote_host("/local/path"), None);
    }
}
