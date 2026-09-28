//! Worktree-aware LSP proxy: `localforest lsp -- <server> [args]`.
//!
//! One editor/agent session (Claude Code in the primary checkout) edits files in many
//! worktrees. A single language server rooted at the primary would index every nested
//! worktree and answer with definitions from the wrong one. The proxy runs one server
//! per worktree instead, rooted there, routes each message by the file it's about,
//! and drops results that point into another checkout of the same repository (files
//! outside the repository, like dependencies elsewhere, pass through).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
};
use tracing::{debug, info, warn};

async fn read_message<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> Result<Option<Value>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = Some(v.trim().parse::<usize>()?);
        }
    }
    let len = len.context("LSP message without Content-Length")?;
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let body = serde_json::to_vec(v)?;
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

pub fn path_to_uri(p: &Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

/// The URI a message or result element is about.
fn uri_of(v: &Value) -> Option<&str> {
    v.get("uri")
        .or_else(|| v.get("targetUri"))
        .or_else(|| v.pointer("/location/uri"))
        .or_else(|| v.pointer("/textDocument/uri"))
        .or_else(|| v.pointer("/item/uri"))
        .and_then(Value::as_str)
}

/// Remove array elements (and WorkspaceEdit `changes` keys) whose URI fails `keep`.
fn filter(v: &mut Value, keep: &dyn Fn(&str) -> bool) {
    match v {
        Value::Array(items) => {
            items.retain(|el| uri_of(el).is_none_or(keep));
            for el in items {
                filter(el, keep);
            }
        }
        Value::Object(map) => {
            if let Some(Value::Object(changes)) = map.get_mut("changes") {
                changes.retain(|k, _| keep(k));
            }
            for (_, x) in map.iter_mut() {
                filter(x, keep);
            }
        }
        _ => {}
    }
}

/// Which checkouts a path belongs to.
#[derive(Clone)]
struct Layout {
    primary: PathBuf,
    /// Linked worktrees, longest path first.
    worktrees: Vec<PathBuf>,
}

impl Layout {
    fn load(primary: &Path) -> Self {
        let mut worktrees: Vec<PathBuf> = crate::worktree::list(primary)
            .map(|l| l.into_iter().map(|i| i.path).collect())
            .unwrap_or_default();
        worktrees.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
        Self {
            primary: primary.to_path_buf(),
            worktrees,
        }
    }

    /// Checkout root owning `path`.
    fn owner(&self, path: &Path) -> PathBuf {
        self.worktrees
            .iter()
            .find(|w| path.starts_with(w))
            .cloned()
            .unwrap_or_else(|| self.primary.clone())
    }

    /// Does a result pointing at `uri` belong in answers from the server for `root`?
    fn keep(&self, root: &Path, uri: &str) -> bool {
        let Some(path) = uri_to_path(uri) else {
            return true;
        };
        let in_repo =
            path.starts_with(&self.primary) || self.worktrees.iter().any(|w| path.starts_with(w));
        !in_repo || self.owner(&path) == root
    }
}

enum Ev {
    Client(Value),
    ClientClosed,
    Server(PathBuf, Value),
    Exited(PathBuf),
}

struct Backend {
    tx: mpsc::UnboundedSender<Value>,
    _child: Child,
    ready: bool,
    queue: Vec<Value>,
}

struct Router {
    cmd: Vec<String>,
    layout: Layout,
    refreshed: Instant,
    backends: HashMap<PathBuf, Backend>,
    init_params: Option<Value>,
    last_config: Option<Value>,
    /// Client request id -> backend handling it.
    pending: HashMap<String, PathBuf>,
    /// Our id for a server->client request -> (backend, its id).
    server_reqs: HashMap<String, (PathBuf, Value)>,
    next: u64,
    active: PathBuf,
    exiting: bool,
    to_client: mpsc::UnboundedSender<Value>,
    events: mpsc::UnboundedSender<Ev>,
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

impl Router {
    fn spawn(&mut self, root: &Path) -> Result<()> {
        let (prog, args) = self
            .cmd
            .split_first()
            .context("no language server command")?;
        let mut child = Command::new(prog)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {prog}"))?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                if write_message(&mut stdin, &v).await.is_err() {
                    break;
                }
            }
        });
        let events = self.events.clone();
        let key = root.to_path_buf();
        tokio::spawn(async move {
            let mut r = BufReader::new(stdout);
            while let Ok(Some(v)) = read_message(&mut r).await {
                if events.send(Ev::Server(key.clone(), v)).is_err() {
                    return;
                }
            }
            let _ = events.send(Ev::Exited(key));
        });
        info!("started {prog} for {}", root.display());
        self.backends.insert(
            root.to_path_buf(),
            Backend {
                tx,
                _child: child,
                ready: false,
                queue: Vec::new(),
            },
        );
        Ok(())
    }

    fn send(&mut self, root: &Path, v: Value) {
        if !self.backends.contains_key(root)
            && let Err(e) = self.start_worktree(root)
        {
            warn!("{e:#}");
            return;
        }
        let b = self.backends.get_mut(root).unwrap();
        if b.ready {
            let _ = b.tx.send(v);
        } else {
            b.queue.push(v);
        }
    }

    fn start_worktree(&mut self, root: &Path) -> Result<()> {
        let Some(mut params) = self.init_params.clone() else {
            bail!("message before initialize");
        };
        self.spawn(root)?;
        let uri = path_to_uri(root);
        params["rootUri"] = json!(uri);
        params["rootPath"] = json!(root.to_string_lossy());
        params["workspaceFolders"] = json!([{
            "uri": uri,
            "name": root.file_name().unwrap_or_default().to_string_lossy(),
        }]);
        self.next += 1;
        let init = json!({"jsonrpc": "2.0", "id": format!("localforest-init:{}", self.next), "method": "initialize", "params": params});
        let _ = self.backends[root].tx.send(init);
        Ok(())
    }

    fn broadcast(&mut self, v: &Value) {
        let roots: Vec<PathBuf> = self.backends.keys().cloned().collect();
        for r in roots {
            self.send(&r, v.clone());
        }
    }

    fn refresh_layout(&mut self, force: bool) {
        if force || self.refreshed.elapsed() > Duration::from_secs(2) {
            self.layout = Layout::load(&self.layout.primary);
            self.refreshed = Instant::now();
        }
    }

    fn route_uri(&mut self, uri: &str, fresh: bool) -> PathBuf {
        self.refresh_layout(fresh);
        match uri_to_path(uri) {
            Some(p) => self.layout.owner(&p),
            None => self.layout.primary.clone(),
        }
    }

    fn on_client(&mut self, v: Value) -> Result<()> {
        let method = v.get("method").and_then(Value::as_str).map(str::to_string);
        let id = v.get("id").cloned();
        let primary = self.layout.primary.clone();
        let Some(method) = method else {
            // Response to a server->client request.
            if let Some(id) = id
                && let Some((root, orig)) = self.server_reqs.remove(&id_key(&id))
            {
                let mut v = v;
                v["id"] = orig;
                self.send(&root, v);
            }
            return Ok(());
        };
        match method.as_str() {
            "initialize" => {
                let params = v.get("params").cloned().unwrap_or(json!({}));
                if let Some(root) = params
                    .get("rootUri")
                    .and_then(Value::as_str)
                    .and_then(uri_to_path)
                    .or_else(|| {
                        params
                            .get("rootPath")
                            .and_then(Value::as_str)
                            .map(PathBuf::from)
                    })
                    && let Ok(p) = crate::config::primary_root(&root)
                {
                    self.layout = Layout::load(&p);
                    self.active = p;
                }
                self.init_params = Some(params);
                let primary = self.layout.primary.clone();
                self.spawn(&primary)?;
                let b = self.backends.get_mut(&primary).unwrap();
                b.ready = true;
                if let Some(id) = id {
                    self.pending.insert(id_key(&id), primary);
                }
                let _ = b.tx.send(v);
            }
            "initialized" => self.send(&primary, v),
            "shutdown" => {
                for (root, b) in &self.backends {
                    if *root != primary && b.ready {
                        let _ = b.tx.send(json!({"jsonrpc": "2.0", "id": "localforest-shutdown", "method": "shutdown"}));
                    }
                }
                if let Some(id) = id {
                    self.pending.insert(id_key(&id), primary.clone());
                }
                self.send(&primary, v);
            }
            "exit" => {
                self.exiting = true;
                self.broadcast(&v);
            }
            "$/cancelRequest" => {
                if let Some(root) = v
                    .pointer("/params/id")
                    .and_then(|id| self.pending.get(&id_key(id)))
                    .cloned()
                {
                    self.send(&root, v);
                }
            }
            "workspace/didChangeConfiguration" => {
                self.last_config = Some(v.clone());
                self.broadcast(&v);
            }
            "workspace/didChangeWatchedFiles" => {
                self.refresh_layout(true);
                let mut by_root: HashMap<PathBuf, Vec<Value>> = HashMap::new();
                for c in v
                    .pointer("/params/changes")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                {
                    let root = c
                        .get("uri")
                        .and_then(Value::as_str)
                        .and_then(uri_to_path)
                        .map_or_else(|| primary.clone(), |p| self.layout.owner(&p));
                    by_root.entry(root).or_default().push(c);
                }
                for (root, changes) in by_root {
                    if self.backends.contains_key(&root) {
                        self.send(&root, json!({"jsonrpc": "2.0", "method": method, "params": {"changes": changes}}));
                    }
                }
            }
            "workspace/didChangeWorkspaceFolders" => self.send(&primary, v),
            _ => {
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                let root = match uri_of(&params) {
                    Some(uri) => {
                        let root = self.route_uri(uri, method == "textDocument/didOpen");
                        self.active = root.clone();
                        root
                    }
                    None => self.active.clone(),
                };
                if let Some(id) = id {
                    self.pending.insert(id_key(&id), root.clone());
                }
                debug!("{method} -> {}", root.display());
                self.send(&root, v);
            }
        }
        Ok(())
    }

    fn on_server(&mut self, root: PathBuf, mut v: Value) {
        let primary = root == self.layout.primary;
        let method = v.get("method").and_then(Value::as_str).map(str::to_string);
        let id = v.get("id").cloned();
        match (method, id) {
            // Response.
            (None, Some(id)) => {
                let key = id.as_str().unwrap_or_default();
                if key.starts_with("localforest-init:") {
                    if let Some(b) = self.backends.get_mut(&root) {
                        b.ready = true;
                        let _ = b
                            .tx
                            .send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
                        if let Some(c) = &self.last_config {
                            let _ = b.tx.send(c.clone());
                        }
                        for m in b.queue.drain(..) {
                            let _ = b.tx.send(m);
                        }
                    }
                    return;
                }
                if key == "localforest-shutdown" {
                    return;
                }
                self.pending.remove(&id_key(&id));
                let layout = self.layout.clone();
                let keep = |uri: &str| layout.keep(&root, uri);
                if let Some(result) = v.get_mut("result") {
                    if uri_of(result).is_some_and(|u| !keep(u)) {
                        *result = Value::Null;
                    } else {
                        filter(result, &keep);
                    }
                }
                let _ = self.to_client.send(v);
            }
            // Server -> client request.
            (Some(method), Some(id)) => {
                if !primary
                    && matches!(
                        method.as_str(),
                        "client/registerCapability"
                            | "client/unregisterCapability"
                            | "window/workDoneProgress/create"
                    )
                {
                    // The client already has these from the primary's server.
                    self.send(&root, json!({"jsonrpc": "2.0", "id": id, "result": null}));
                    return;
                }
                self.next += 1;
                let ours = json!(format!("localforest:{}", self.next));
                self.server_reqs.insert(id_key(&ours), (root, id));
                v["id"] = ours;
                let _ = self.to_client.send(v);
            }
            // Notification.
            (Some(method), None) => {
                if method == "$/progress" && !primary {
                    return;
                }
                if method == "textDocument/publishDiagnostics"
                    && let Some(uri) = v.pointer("/params/uri").and_then(Value::as_str)
                    && !self.layout.keep(&root, uri)
                {
                    return;
                }
                let _ = self.to_client.send(v);
            }
            (None, None) => {}
        }
    }
}

pub async fn run(cmd: Vec<String>) -> Result<()> {
    if cmd.is_empty() {
        bail!("usage: localforest lsp -- <language server> [args]");
    }
    let cwd = std::env::current_dir()?;
    let primary = crate::config::primary_root(&cwd).unwrap_or(cwd);
    let (to_client, mut client_rx) = mpsc::unbounded_channel::<Value>();
    let (events, mut ev_rx) = mpsc::unbounded_channel::<Ev>();

    tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(v) = client_rx.recv().await {
            if write_message(&mut out, &v).await.is_err() {
                break;
            }
        }
    });
    let client_events = events.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(tokio::io::stdin());
        loop {
            match read_message(&mut r).await {
                Ok(Some(v)) => {
                    if client_events.send(Ev::Client(v)).is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => {
                    let _ = client_events.send(Ev::ClientClosed);
                    return;
                }
            }
        }
    });

    let mut router = Router {
        cmd,
        layout: Layout::load(&primary),
        refreshed: Instant::now(),
        backends: HashMap::new(),
        init_params: None,
        last_config: None,
        pending: HashMap::new(),
        server_reqs: HashMap::new(),
        next: 0,
        active: primary,
        exiting: false,
        to_client,
        events,
    };
    while let Some(ev) = ev_rx.recv().await {
        match ev {
            Ev::Client(v) => {
                if let Err(e) = router.on_client(v) {
                    warn!("{e:#}");
                }
                if router.exiting {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    return Ok(());
                }
            }
            Ev::ClientClosed => return Ok(()),
            Ev::Server(root, v) => router.on_server(root, v),
            Ev::Exited(root) => {
                router.backends.remove(&root);
                if root == router.layout.primary {
                    if router.exiting {
                        return Ok(());
                    }
                    bail!("language server for {} exited", root.display());
                }
                info!("language server for {} exited", root.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout {
            primary: "/p".into(),
            worktrees: vec![
                "/p/.claude/worktrees/a".into(),
                "/p/.claude/worktrees/b".into(),
            ],
        }
    }

    #[test]
    fn uris_roundtrip() {
        let p = Path::new("/a b/c#d.ex");
        assert_eq!(path_to_uri(p), "file:///a%20b/c%23d.ex");
        assert_eq!(uri_to_path(&path_to_uri(p)).unwrap(), p);
    }

    #[test]
    fn keeps_only_own_checkout() {
        let l = layout();
        let a = Path::new("/p/.claude/worktrees/a");
        assert!(l.keep(a, "file:///p/.claude/worktrees/a/lib/x.ex"));
        assert!(!l.keep(a, "file:///p/lib/x.ex"));
        assert!(!l.keep(a, "file:///p/.claude/worktrees/b/lib/x.ex"));
        assert!(l.keep(a, "file:///nix/store/elixir/lib/enum.ex"));
        let p = Path::new("/p");
        assert!(l.keep(p, "file:///p/lib/x.ex"));
        assert!(!l.keep(p, "file:///p/.claude/worktrees/a/lib/x.ex"));
    }

    #[test]
    fn filters_results() {
        let l = layout();
        let root = PathBuf::from("/p/.claude/worktrees/a");
        let mut v = json!([
            {"uri": "file:///p/.claude/worktrees/a/x.ex", "range": {}},
            {"uri": "file:///p/x.ex", "range": {}},
            {"name": "S", "location": {"uri": "file:///p/.claude/worktrees/b/x.ex"}},
        ]);
        filter(&mut v, &|u| l.keep(&root, u));
        assert_eq!(v.as_array().unwrap().len(), 1);
        let mut edit =
            json!({"changes": {"file:///p/x.ex": [], "file:///p/.claude/worktrees/a/x.ex": []}});
        filter(&mut edit, &|u| l.keep(&root, u));
        assert_eq!(edit["changes"].as_object().unwrap().len(), 1);
    }
}
