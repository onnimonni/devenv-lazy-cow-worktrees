//! Background tasks of a registered project: its job worker, the safety-net ticker, its domain's certificate and the GitHub webhook watcher.

use super::*;

pub(super) fn github_client(project: &Project) -> Result<(github::Client, String)> {
    let url = Repository::open(&project.root)?
        .find_remote(&project.settings.remote)?
        .url()
        .map(str::to_string)
        .unwrap_or_default();
    let repo =
        github::parse_remote_url(&url).ok_or_else(|| anyhow!("{url:?} is not a GitHub remote"))?;
    let token = github::auth_token(&repo.host, &project.env)?;
    Ok((github::Client::new(repo, token.clone())?, token))
}

/// Runs pending jobs for one project, debounced so bursts collapse.
pub(super) async fn worker(d: Arc<Daemon>, rt: Arc<ProjectRt>) {
    loop {
        rt.wake.notified().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let take = |f: &AtomicBool| f.swap(false, Ordering::SeqCst);
        // Merged worktrees go before sync merges the base branch into them.
        if take(&rt.pending.merged)
            && let Err(e) = d.remove_merged(&rt).await
        {
            warn!("{}: checking merged PRs: {e:#}", rt.project.name);
        }
        if take(&rt.pending.sync) {
            if let Err(e) = d.sync(&rt).await {
                error!("{}: sync failed: {e:#}", rt.project.name);
            }
            rt.pending.migrate.store(true, Ordering::SeqCst);
        }
        if take(&rt.pending.migrate)
            && let Err(e) = d.migrate(&rt, false).await
        {
            error!("{}: migrate: {e:#}", rt.project.name);
        }
        if take(&rt.pending.reconcile)
            && let Err(e) = d.reconcile(&rt).await
        {
            warn!("{}: {e:#}", rt.project.name);
        }
        if take(&rt.pending.migrate_worktrees) {
            d.migrate_worktrees(&rt).await;
        }
    }
}

/// Safety net for missed events: every minute without the websocket, else every 10.
pub(super) async fn ticker(rt: Arc<ProjectRt>) {
    let mut n: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        n += 1;
        // Retries failed migrations once their backoff is over.
        if !rt.migrate_failures.lock().is_empty() {
            rt.trigger(&rt.pending.migrate_worktrees);
        }
        // And the primary's (setup included).
        if rt
            .migrate_failure(&rt.primary())
            .is_some_and(|f| !f.backing_off())
        {
            rt.trigger(&rt.pending.migrate);
        }
        // Retries checkouts whose provisioning failed.
        rt.trigger(&rt.pending.reconcile);
        if !rt.ws_ok.load(Ordering::SeqCst) || n.is_multiple_of(10) {
            rt.pending.merged.store(true, Ordering::SeqCst);
            rt.trigger(&rt.pending.sync);
        }
    }
}

/// Its domain's certificate and key, from the GitHub repository's artifact (only
/// while it's private), for the proxy: read now and every hour, so a renewal by the
/// repository's workflow reaches the proxy within one.
pub(super) async fn domain_certificate(trusted: Arc<Trusted>, rt: Arc<ProjectRt>) {
    let p = &rt.project;
    let Some(domain) = p.settings.tls_domain.clone() else {
        return;
    };
    let mut current: Option<Vec<u8>> = None;
    loop {
        let run = async {
            let gh = tls::trusted::repo_client(p)?;
            if !gh.is_private().await? {
                trusted.clear(&domain);
                current = None;
                anyhow::bail!(
                    "{} is public: stopped serving its certificates and won't read them",
                    gh.repo
                );
            }
            let Some(zip) = gh
                .artifact(tls::trusted::ARTIFACT, tls::trusted::MAX_ARTIFACT_BYTES)
                .await?
            else {
                anyhow::bail!(
                    "no {} artifact in {} yet: set up github.com/onnimonni/trusted-https-certificate-to-artifacts-action there",
                    tls::trusted::ARTIFACT,
                    gh.repo
                );
            };
            if current.as_ref() == Some(&zip) {
                return Ok(());
            }
            let certs = tls::trusted::certificates(&zip)
                .with_context(|| format!("{} of {}", tls::trusted::ARTIFACT, gh.repo))?;
            let now = tls::trusted::now();
            let missing: Vec<_> = p
                .tls_names()
                .into_iter()
                .filter(|n| {
                    !certs
                        .iter()
                        .any(|c| c.leaf.valid_at(now) && c.leaf.covers(n))
                })
                .collect();
            if !missing.is_empty() {
                warn!(
                    "{}: the certificates lack {} (the local CA serves them): `lazy-cow-tree cert show`",
                    p.name,
                    missing.join(", ")
                );
            }
            for c in &certs {
                info!(
                    "{}: {} for {} until {}",
                    p.name,
                    c.file,
                    c.leaf.names.join(", "),
                    time::OffsetDateTime::from_unix_timestamp(c.leaf.not_after)
                        .map(|t| t.date().to_string())
                        .unwrap_or_default()
                );
            }
            trusted.set(&domain, certs);
            current = Some(zip);
            anyhow::Ok(())
        };
        let minutes = match run.await {
            Ok(()) => 60,
            Err(e) => {
                warn!("{}: {domain} certificate: {e:#}", p.name);
                10
            }
        };
        tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
    }
}

/// Keep a webhook websocket open, recreating the hook on disconnect.
pub(super) async fn watch_github(rt: Arc<ProjectRt>, gh: Arc<github::Client>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let hook = match gh.create_hook().await {
            Ok(h) => h,
            Err(e) => {
                rt.ws_ok.store(false, Ordering::SeqCst);
                warn!(
                    "{}: {e:#}; polling every minute instead, retrying in 10 min",
                    rt.project.name
                );
                tokio::time::sleep(Duration::from_secs(600)).await;
                continue;
            }
        };
        let guard = HookGuard {
            client: gh.clone(),
            hook: Some(hook),
        };
        rt.trigger(&rt.pending.sync);
        let started = tokio::time::Instant::now();
        let base_ref = format!("refs/heads/{}", rt.base);
        rt.ws_ok.store(true, Ordering::SeqCst);
        let result = gh
            .stream_events(guard.hook(), |ev| match ev {
                github::Event::Push(r) => {
                    info!("{}: push to {r}", rt.project.name);
                    if r == base_ref {
                        rt.pending.merged.store(true, Ordering::SeqCst);
                    }
                    rt.trigger(&rt.pending.sync);
                }
                github::Event::Merged { number, branch } => {
                    info!("{}: #{number} ({branch}) merged", rt.project.name);
                    rt.trigger(&rt.pending.merged);
                }
            })
            .await;
        rt.ws_ok.store(false, Ordering::SeqCst);
        guard.cleanup().await;
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        match result {
            Ok(()) => warn!(
                "{}: websocket closed; reconnecting in {backoff:?}",
                rt.project.name
            ),
            Err(e) => warn!(
                "{}: websocket: {e:#}; reconnecting in {backoff:?}",
                rt.project.name
            ),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

/// Deletes the hook on cleanup, or in the background when dropped (task aborted).
pub(super) struct HookGuard {
    client: Arc<github::Client>,
    hook: Option<github::Hook>,
}

impl HookGuard {
    fn hook(&self) -> &github::Hook {
        self.hook.as_ref().expect("hook present until cleanup")
    }

    async fn cleanup(mut self) {
        if let Some(h) = self.hook.take()
            && let Err(e) = self.client.delete_hook(&h).await
        {
            warn!("{e:#}");
        }
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        if let Some(h) = self.hook.take() {
            let client = self.client.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    if let Err(e) = client.delete_hook(&h).await {
                        warn!("{e:#}");
                    }
                });
            }
        }
    }
}
