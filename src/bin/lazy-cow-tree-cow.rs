//! lazy-cow-tree-cow: fills a fresh `git worktree add --no-checkout` worktree like
//! `lazy-cow-tree worktree new` does. Run by the devenv module's `git` wrapper; no
//! daemon needed.

#[path = "../cow.rs"]
mod cow;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use git2::Repository;

/// Copy-on-write git worktrees for lazy-cow-tree.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fill a fresh `git worktree add --no-checkout` worktree with copy-on-write clones
    /// of the primary checkout, build caches included (language server indexes left
    /// behind); copies the caches where the filesystem can't clone.
    Populate {
        /// Only print warnings.
        #[arg(short, long)]
        quiet: bool,
        worktree: PathBuf,
    },
}

fn main() -> Result<()> {
    let Cmd::Populate { quiet, worktree } = Cli::parse().command;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .with_max_level(if quiet {
            tracing::Level::WARN
        } else {
            tracing::Level::INFO
        })
        .init();
    let path = worktree
        .canonicalize()
        .with_context(|| format!("no worktree at {}", worktree.display()))?;
    let repo = Repository::open(&path)?;
    let root = Repository::open(repo.commondir())?
        .workdir()
        .context("bare repositories are not supported")?
        .canonicalize()?;
    anyhow::ensure!(root != path, "{} is the primary checkout", path.display());
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let generated = std::env::var(cow::GENERATED_FILES_ENV).unwrap_or_default();
    cow::populate(&root, &path, &name, &generated)
}
