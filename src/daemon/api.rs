//! The control API on the daemon's unix socket, and the dashboard at https://lazy-cow-tree.localhost.

use super::*;

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

pub(super) fn api(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/status", get(|State(d): State<Arc<Daemon>>| async move { ApiResult::Ok(Json(d.status().await?)) }))
        .route(
            "/projects",
            post(|State(d): State<Arc<Daemon>>, Json(p): Json<Project>| async move {
                d.register(p).await?;
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/projects/remove",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RootReq>| async move {
                let root = r.root.canonicalize().unwrap_or(r.root);
                d.projects.lock().remove(&root);
                d.save();
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/worktrees",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<CreateReq>| async move {
                ApiResult::Ok(Json(d.create_worktree(r).await?))
            }),
        )
        .route(
            "/worktrees/remove",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RemoveReq>| async move {
                let warnings = d.remove_worktree(r).await?;
                ApiResult::Ok(Json(serde_json::json!({ "warnings": warnings })))
            }),
        )
        .route(
            "/reconcile",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RootReq>| async move {
                let rt = d.project(&r.root)?;
                d.reconcile(&rt).await?;
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/sync",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RootReq>| async move {
                let rt = d.project(&r.root)?;
                d.remove_merged(&rt).await?;
                d.sync(&rt).await?;
                d.migrate(&rt, true).await?;
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/db/snapshot",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RootReq>| async move {
                let rt = d.project(&r.root)?;
                let _g = rt.lock.lock().await;
                let primary = rt.primary();
                for kind in primary.db_kinds() {
                    d.pg
                        .snapshot(
                            &primary.dev_db_of(kind),
                            &primary.template_db_of(kind),
                            &d.create_lock,
                        )
                        .await?;
                }
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/service/start",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<ServiceReq>| async move {
                let (rt, c, svc) =
                    d.service_named(&r.root, r.worktree.as_deref(), r.service.as_deref())?;
                d.ensure_service(&rt, &c, &svc, None).await?;
                ApiResult::Ok(Json(serde_json::json!({
                    "log": crate::server::log_path(&c.service_id(&svc)),
                    "url": c.service(&svc).filter(|s| s.http).map(|_| format!("https://{}", c.service_host(&svc))),
                })))
            }),
        )
        .route(
            "/service/stop",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<ServiceReq>| async move {
                let (_, c, svc) =
                    d.service_named(&r.root, r.worktree.as_deref(), r.service.as_deref())?;
                d.servers.stop(&c.service_id(&svc)).await;
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/service/log",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<ServiceReq>| async move {
                let (_, c, svc) =
                    d.service_named(&r.root, r.worktree.as_deref(), r.service.as_deref())?;
                ApiResult::Ok(Json(serde_json::json!({
                    "log": crate::server::log_path(&c.service_id(&svc)),
                })))
            }),
        )
        .route(
            "/shutdown",
            post(|State(d): State<Arc<Daemon>>| async move {
                d.shutdown.notify_one();
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .with_state(d)
}

pub(super) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(super) fn dashboard(d: &Daemon) -> String {
    let mut rows = String::new();
    let projects: Vec<Arc<ProjectRt>> = d.projects.lock().values().cloned().collect();
    for rt in projects {
        let mut checkouts = vec![rt.primary()];
        checkouts.extend(rt.known.lock().values().cloned());
        for c in checkouts {
            let links: Vec<String> = c
                .routes()
                .iter()
                .map(|(svc, host, port)| {
                    let host = html_escape(host);
                    let label = svc
                        .as_deref()
                        .map(|s| format!(" ({})", html_escape(s)))
                        .unwrap_or_default();
                    format!("<a href=\"https://{host}\">{host}</a>{label} :{port}")
                })
                .collect();
            rows.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td><td><code>{}</code></td></tr>",
                html_escape(&rt.project.name),
                html_escape(c.worktree.as_deref().unwrap_or("(primary)")),
                links.join("<br>"),
                html_escape(&c.dev_db()),
                html_escape(&c.path.display().to_string()),
            ));
        }
    }
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>lazy-cow-tree</title>
<style>body{{font:14px system-ui;margin:2em;color:#222;background:#fff}}table{{border-collapse:collapse}}td,th{{padding:.3em .8em;border-bottom:1px solid #ddd;text-align:left}}
@media(prefers-color-scheme:dark){{body{{color:#ddd;background:#111}}a{{color:#8af}}td,th{{border-color:#333}}}}</style></head>
<body><h1>lazy-cow-tree</h1><p>PostgreSQL 127.0.0.1:{} · Redis 127.0.0.1:{} (password = checkout)</p>
<table><tr><th>project</th><th>worktree</th><th>services</th><th>databases</th><th>path</th></tr>{rows}</table></body></html>"#,
        d.global.pg_port, d.global.redis_port
    )
}
