//! Removed worktrees, remembered in the main checkout's git dir
//! (`.git/lazy-cow-tree/worktrees.json`, shared by every worktree, never committed), so a
//! request to a removed worktree's hostname can explain why it's gone and bring it
//! back as a preview. Also links to the forge (GitHub, GitLab, …) the remote is on.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Result;
use git2::Repository;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// Its pull request merged (auto-removed, or `worktree rm` proved it).
    Merged,
    /// `lazy-cow-tree worktree rm` (or Claude Code's WorktreeRemove hook).
    Removed,
    /// Its directory was deleted by hand.
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    pub branch: Option<String>,
    /// Commit it was at (hex), if known.
    pub head: Option<String>,
    /// Unix seconds.
    pub at: u64,
    pub reason: Reason,
    pub pr: Option<u64>,
    /// Recreated as a preview since, and closed again (unix seconds).
    #[serde(default)]
    pub preview_closed: Option<u64>,
    /// Gitignored files deleted with it (not build caches or copies of the primary's).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost: Vec<String>,
}

/// A worktree recreated from `removed` to preview its branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub since: u64,
    /// Last request to its hostnames or connection to its database / Redis (unix s).
    pub last_active: u64,
    /// Why it was gone before (restored when the preview closes).
    pub original: Removed,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    /// By worktree name.
    #[serde(default)]
    pub removed: BTreeMap<String, Removed>,
    /// Previews by worktree name: never auto-removed for their merged PR, closed
    /// after `previewTtlHours` without activity.
    #[serde(default)]
    pub previews: BTreeMap<String, Preview>,
}

fn path(root: &Path) -> Result<PathBuf> {
    Ok(Repository::open(root)?
        .commondir()
        .join("lazy-cow-tree/worktrees.json"))
}

/// Last activity per checkout id (unix seconds), in memory: HTTPS requests to its
/// hostnames, connections to its database and Redis.
#[derive(Clone, Default)]
pub struct Activity(std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>);

impl Activity {
    pub fn touch(&self, id: &str) {
        self.0.lock().unwrap().insert(id.to_string(), now());
    }

    pub fn get(&self, id: &str) -> Option<u64> {
        self.0.lock().unwrap().get(id).copied()
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl History {
    pub fn load(root: &Path) -> Self {
        path(root)
            .ok()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self, root: &Path) -> Result<()> {
        let p = path(root)?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    /// Load, change and save.
    pub fn update(root: &Path, f: impl FnOnce(&mut History)) -> Result<()> {
        let mut h = Self::load(root);
        f(&mut h);
        h.save(root)
    }
}

/// Web pages of the repository behind a git remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forge {
    /// "GitHub", "GitLab", …
    pub name: String,
    /// https://host/owner/repo
    pub repo: String,
    kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    GitHub,
    GitLab,
    Bitbucket,
    Gitea,
}

impl Forge {
    /// From `git@host:group/sub/repo.git`, `https://host/o/r`, `ssh://git@host:22/o/r`.
    pub fn from_remote(url: &str) -> Option<Self> {
        let url = url.trim().trim_end_matches('/');
        let url = url.strip_suffix(".git").unwrap_or(url);
        let (host, path) = if let Some((_, rest)) = url.split_once("://") {
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            (host.split(':').next()?, path)
        } else {
            let (authority, path) = url.split_once(':')?;
            (
                authority.rsplit_once('@').map_or(authority, |(_, h)| h),
                path,
            )
        };
        let path = path.trim_matches('/');
        if host.is_empty() || !path.contains('/') {
            return None;
        }
        let (kind, name) = if host.contains("github") {
            (Kind::GitHub, "GitHub".to_string())
        } else if host.contains("gitlab") {
            (Kind::GitLab, "GitLab".to_string())
        } else if host.contains("bitbucket") {
            (Kind::Bitbucket, "Bitbucket".to_string())
        } else if host.contains("codeberg") {
            (Kind::Gitea, "Codeberg".to_string())
        } else if host.contains("gitea") || host.contains("forgejo") {
            (Kind::Gitea, "Gitea".to_string())
        } else {
            (Kind::GitHub, host.to_string())
        };
        Some(Self {
            name,
            repo: format!("https://{host}/{path}"),
            kind,
        })
    }

    pub fn pr(&self, n: u64) -> String {
        match self.kind {
            Kind::GitHub => format!("{}/pull/{n}", self.repo),
            Kind::GitLab => format!("{}/-/merge_requests/{n}", self.repo),
            Kind::Bitbucket => format!("{}/pull-requests/{n}", self.repo),
            Kind::Gitea => format!("{}/pulls/{n}", self.repo),
        }
    }

    pub fn pr_label(&self, n: u64) -> String {
        match self.kind {
            Kind::GitLab => format!("merge request !{n}"),
            _ => format!("pull request #{n}"),
        }
    }

    pub fn branch(&self, b: &str) -> String {
        match self.kind {
            Kind::GitHub => format!("{}/tree/{b}", self.repo),
            Kind::GitLab => format!("{}/-/tree/{b}", self.repo),
            Kind::Bitbucket => format!("{}/branch/{b}", self.repo),
            Kind::Gitea => format!("{}/src/branch/{b}", self.repo),
        }
    }

    pub fn commit(&self, sha: &str) -> String {
        match self.kind {
            Kind::GitHub | Kind::Gitea => format!("{}/commit/{sha}", self.repo),
            Kind::GitLab => format!("{}/-/commit/{sha}", self.repo),
            Kind::Bitbucket => format!("{}/commits/{sha}", self.repo),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_links() {
        let gh = Forge::from_remote("git@github.com:o/app.git").unwrap();
        assert_eq!(gh.name, "GitHub");
        assert_eq!(gh.pr(7), "https://github.com/o/app/pull/7");
        assert_eq!(gh.branch("fix"), "https://github.com/o/app/tree/fix");
        let gl = Forge::from_remote("https://gitlab.com/group/sub/app.git").unwrap();
        assert_eq!(
            gl.pr(7),
            "https://gitlab.com/group/sub/app/-/merge_requests/7"
        );
        assert_eq!(
            gl.commit("abc"),
            "https://gitlab.com/group/sub/app/-/commit/abc"
        );
        let cb = Forge::from_remote("ssh://git@codeberg.org:22/o/app").unwrap();
        assert_eq!(cb.pr(3), "https://codeberg.org/o/app/pulls/3");
        assert!(Forge::from_remote("/srv/git/app.git").is_none());
    }

    #[test]
    fn history_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        Repository::init(dir.path()).unwrap();
        let rec = Removed {
            branch: Some("fix".into()),
            head: Some("abc".into()),
            at: 1,
            reason: Reason::Merged,
            pr: Some(7),
            preview_closed: None,
            lost: vec!["notes.local".into()],
        };
        History::update(dir.path(), |h| {
            h.removed.insert("fix".into(), rec.clone());
            h.previews.insert(
                "other".into(),
                Preview {
                    since: 2,
                    last_active: 3,
                    original: rec.clone(),
                },
            );
        })
        .unwrap();
        let h = History::load(dir.path());
        assert_eq!(h.removed["fix"], rec);
        assert_eq!(h.previews["other"].last_active, 3);
    }
}
