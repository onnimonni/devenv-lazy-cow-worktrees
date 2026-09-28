//! `localforest lsp -- dexter lsp` answers each worktree from that worktree only.
//!
//! A primary checkout and two worktrees each define `Foo.hello` on a different line;
//! go-to-definition from `Bar` (and workspace symbols, references) must stay in the
//! checkout the request came from. Needs `dexter` (the Elixir LSP) in PATH or
//! `$DEXTER`; skipped otherwise.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use git2::{Repository, Signature, WorktreeAddOptions};
use serde_json::{Value, json};

fn dexter() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DEXTER") {
        return Some(d.into());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("dexter"))
        .find(|p| p.is_file())
}

fn uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

/// `Foo.hello` on line `line` (0-based), returning `tag`.
fn foo(tag: &str, line: usize) -> String {
    format!(
        "defmodule Foo do\n{}  def hello, do: :{tag}\nend\n",
        "\n".repeat(line - 1)
    )
}

const BAR: &str = "defmodule Bar do\n  def call, do: Foo.hello()\nend\n";

fn commit_all(repo: &Repository, msg: &str) {
    let mut idx = repo.index().unwrap();
    idx.add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = Signature::now("t", "t@t").unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
        .unwrap();
}

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next: u64,
}

impl Lsp {
    fn send(&mut self, v: Value) {
        let body = serde_json::to_vec(&v).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        self.stdin.write_all(&body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Send a request and wait for its result, answering the server's own requests.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let msg = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("no answer to {method}"));
            if msg.get("method").is_some() {
                if let Some(sid) = msg.get("id") {
                    // workspace/configuration and friends: an empty answer.
                    let items = msg
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map(Vec::len);
                    let result = items.map_or(Value::Null, |n| json!(vec![Value::Null; n]));
                    self.send(json!({"jsonrpc": "2.0", "id": sid, "result": result}));
                }
                continue;
            }
            if msg.get("id") == Some(&json!(id)) {
                return msg.get("result").cloned().unwrap_or(Value::Null);
            }
        }
    }
}

fn start(primary: &Path, dexter: &Path) -> Lsp {
    let mut child = Command::new(env!("CARGO_BIN_EXE_localforest"))
        .args(["lsp", "--"])
        .arg(dexter)
        .arg("lsp")
        .current_dir(primary)
        .env("RUST_LOG", "localforest=debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let mut len = 0;
            loop {
                let mut line = String::new();
                if out.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some(v) = line.strip_prefix("Content-Length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            if out.read_exact(&mut body).is_err() {
                return;
            }
            if tx.send(serde_json::from_slice(&body).unwrap()).is_err() {
                return;
            }
        }
    });
    Lsp {
        child,
        stdin,
        rx,
        next: 0,
    }
}

/// Definitions of `Foo.hello` asked from `Bar` in `checkout`, retried while the
/// server is still indexing.
fn definition(lsp: &mut Lsp, checkout: &Path) -> Vec<(String, u64)> {
    let bar = checkout.join("lib/bar.ex");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let result = lsp.request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri(&bar)},
                // `hello` in `Foo.hello()` on line 1.
                "position": {"line": 1, "character": 21},
            }),
        );
        let locations: Vec<Value> = match result {
            Value::Array(a) => a,
            Value::Null => Vec::new(),
            one => vec![one],
        };
        let found: Vec<(String, u64)> = locations
            .iter()
            .filter_map(|l| {
                let uri = l.get("uri").or_else(|| l.get("targetUri"))?.as_str()?;
                let line = l
                    .pointer("/range/start/line")
                    .or_else(|| l.pointer("/targetSelectionRange/start/line"))?
                    .as_u64()?;
                Some((uri.to_string(), line))
            })
            .collect();
        if !found.is_empty() || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn dexter_answers_each_worktree_from_itself() {
    let Some(dexter) = dexter() else {
        eprintln!("skipping: no dexter in PATH (or $DEXTER)");
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let primary = tmp.path().canonicalize().unwrap().join("app");
    std::fs::create_dir_all(primary.join("lib")).unwrap();
    std::fs::write(
        primary.join("mix.exs"),
        "defmodule App.MixProject do\n  use Mix.Project\n  def project, do: [app: :app, version: \"0.1.0\"]\nend\n",
    )
    .unwrap();
    std::fs::write(primary.join(".gitignore"), ".dexter/\n").unwrap();
    std::fs::write(primary.join("lib/foo.ex"), foo("primary", 1)).unwrap();
    std::fs::write(primary.join("lib/bar.ex"), BAR).unwrap();
    let repo = Repository::init(&primary).unwrap();
    commit_all(&repo, "init");
    let head = repo.head().unwrap().peel_to_commit().unwrap();

    // Worktrees where the agent tools put them: nested in the primary checkout.
    let mut checkouts = vec![(primary.clone(), 1)];
    for (name, line) in [("wt-a", 3), ("wt-b", 5)] {
        let path = primary.join(".claude/worktrees").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let branch = repo.branch(name, &head, false).unwrap();
        let r = branch.into_reference();
        repo.worktree(
            name,
            &path,
            Some(WorktreeAddOptions::new().reference(Some(&r))),
        )
        .unwrap();
        std::fs::write(path.join("lib/foo.ex"), foo(name, line)).unwrap();
        commit_all(&Repository::open(&path).unwrap(), name);
        checkouts.push((path, line));
    }

    let mut lsp = start(&primary, &dexter);
    lsp.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&primary),
            "capabilities": {"workspace": {"configuration": true, "workspaceFolders": true}},
            "workspaceFolders": [{"uri": uri(&primary), "name": "app"}],
        }),
    );
    lsp.notify("initialized", json!({}));

    // Worktrees first: the primary's own server must not answer for them.
    for (checkout, line) in checkouts.iter().rev() {
        let bar = checkout.join("lib/bar.ex");
        lsp.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri(&bar), "languageId": "elixir", "version": 1, "text": BAR}}),
        );
        let found = definition(&mut lsp, checkout);
        let own = uri(&checkout.join("lib/foo.ex"));
        assert_eq!(
            found,
            vec![(own, *line as u64)],
            "definition of Foo.hello from {}",
            checkout.display()
        );
    }

    // Workspace symbols and references: only the active (last used: primary) checkout.
    let symbols = lsp.request("workspace/symbol", json!({"query": "Foo"}));
    let refs = lsp.request(
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&primary.join("lib/foo.ex"))},
            "position": {"line": 1, "character": 7},
            "context": {"includeDeclaration": true},
        }),
    );
    for v in [&symbols, &refs] {
        let text = v.to_string();
        assert!(
            !text.contains(".claude/worktrees"),
            "primary answer mentions a worktree: {text}"
        );
    }

    lsp.request("shutdown", Value::Null);
    lsp.notify("exit", Value::Null);
    let _ = lsp.child.wait();
}
