use super::*;
use crate::config::Services;

/// A one-shot HTTP server answering every connection with `status`.
async fn answer(status: u16) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await;
            let _ = s
                .write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\n\r\n").as_bytes())
                .await;
        }
    });
    port
}

#[tokio::test]
async fn ready_probe() {
    assert!(http_ready(answer(200).await, "/readyz").await);
    assert!(http_ready(answer(302).await, "/").await);
    assert!(!http_ready(answer(503).await, "/").await);
    // Nothing listening.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    assert!(!http_ready(port, "/").await);
}

#[test]
fn scripts_naming_the_primary_run_as_rewritten_copies() {
    let d = tempfile::tempdir().unwrap();
    let (root, wt) = (
        Path::new("/src/app"),
        Path::new("/src/app/.claude/worktrees/wt"),
    );
    let script = d.path().join("lazy-cow-tree-web");
    std::fs::write(&script, "#!/bin/sh\ncd /src/app/api\nexec mix phx.server\n").unwrap();
    let out = d.path().join("scripts");
    let copy = rewritten_script(&script, root, wt, &out).unwrap();
    assert!(copy.starts_with(&out));
    assert_eq!(
        std::fs::read_to_string(&copy).unwrap(),
        "#!/bin/sh\ncd /src/app/.claude/worktrees/wt/api\nexec mix phx.server\n"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&copy).unwrap().permissions().mode() & 0o111,
        0o111
    );
    // Same content: same copy.
    assert_eq!(rewritten_script(&script, root, wt, &out), Some(copy));
    // Nothing to rewrite: runs as is.
    std::fs::write(&script, "#!/bin/sh\nexec true\n").unwrap();
    assert_eq!(rewritten_script(&script, root, wt, &out), None);
}

#[test]
fn worktree_commands_get_its_paths_and_devenv_vars() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("app");
    let wt = root.join(".claude/worktrees/wt");
    std::fs::create_dir_all(&wt).unwrap();
    let mut project = Project::new(
        root.clone(),
        serde_json::from_value(serde_json::json!({
            "name": "app", "port": 4000, "remote": "origin", "base": null,
            "worktrees_dir": ".claude/worktrees",
            "migrate": null, "seed": null, "setup": null, "services": {},
            "no_sync": false, "no_auto_remove": false
        }))
        .unwrap(),
    );
    let r = root.display().to_string();
    project.env = vec![
        ("PATH".into(), format!("{r}/bin:/usr/bin:/bin")),
        ("DEVENV_ROOT".into(), r.clone()),
        ("PGDATA".into(), format!("{r}/.devenv/state/postgres")),
        ("REDISDATA".into(), format!("{r}/.devenv/state/redis")),
    ];
    let cmd = command(
        &project,
        &wt,
        "echo hi",
        &root.join("api"),
        vec![
            ("PORT".into(), "20000".into()),
            ("CFG".into(), format!("{r}/config")),
        ],
    )
    .unwrap();
    let std = cmd.as_std();
    let env: BTreeMap<String, String> = std
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
        .collect();
    let w = wt.display().to_string();
    assert_eq!(std.get_current_dir(), Some(wt.join("api").as_path()));
    assert_eq!(env["PATH"], format!("{w}/bin:/usr/bin:/bin"));
    assert_eq!(env["DEVENV_ROOT"], w);
    assert_eq!(env["DEVENV_STATE"], format!("{w}/.devenv/state"));
    assert!(env["DEVENV_RUNTIME"].starts_with("/tmp/lazy-cow-tree-"));
    assert_eq!(env["CFG"], format!("{w}/config"));
    assert_eq!(env["PORT"], "20000");
    // devenv's own postgres/redis state never reaches what lazy-cow-tree runs.
    assert!(!env.contains_key("PGDATA") && !env.contains_key("REDISDATA"));

    // The primary keeps its paths.
    let cmd = command(&project, &root, "echo hi", &root, vec![]).unwrap();
    let env: BTreeMap<String, String> = cmd
        .as_std()
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
        .collect();
    assert_eq!(env["DEVENV_ROOT"], r);
    let _ = std::fs::remove_dir(&devenv_vars(&wt)[3].1);
}

#[test]
fn dependencies_first() {
    let c = Checkout {
        project: "p".into(),
        db_prefix: "p".into(),
        worktree: None,
        path: "/x".into(),
        port: 4000,
        services: r#"{"web": {"exec": "a", "dependsOn": ["worker", "api"]},
                      "api": {"exec": "b", "dependsOn": ["worker"]},
                      "worker": {"exec": "c", "http": false}}"#
            .parse::<Services>()
            .unwrap(),
        extra_dbs: Vec::new(),
    };
    let mut order = Vec::new();
    visit(&c, "web", &mut BTreeSet::new(), &mut order).unwrap();
    assert_eq!(order, ["worker", "api", "web"]);

    let cyclic = Checkout {
        services:
            r#"{"a": {"exec": "x", "dependsOn": ["b"]}, "b": {"exec": "y", "dependsOn": ["a"]}}"#
                .parse()
                .unwrap(),
        ..c
    };
    assert!(visit(&cyclic, "a", &mut BTreeSet::new(), &mut Vec::new()).is_err());
}

#[test]
fn restart_on_change_defaults() {
    assert!(is_mix("mix phx.server"));
    assert!(is_mix("iex -S mix phx.server"));
    assert!(is_mix("with-secrets -- /nix/store/x/bin/mix run --no-halt"));
    assert!(!is_mix("bun run dev"));
    assert!(is_mix("sh -c 'mix assets.build && mix phx.server'"));
    assert!(!is_mix("mixer serve"));
    assert!(is_dependency_file(Path::new("/a/mix.lock")));
    assert!(is_dependency_file(Path::new("/a/Gemfile.lock")));
    assert!(!is_dependency_file(Path::new("/a/config/dev.exs")));

    let services: Services = r#"{"web": {"exec": "mix phx.server"},
                                 "off": {"exec": "mix phx.server", "restartOnChange": []},
                                 "rails": {"exec": "bin/rails s", "restartOnChange": ["Gemfile.lock"]},
                                 "vite": {"exec": "bun run dev"}}"#
        .parse()
        .unwrap();
    let patterns = |n: &str| restart_patterns(&services.0[n]);
    assert_eq!(patterns("web"), MIX_FILES);
    assert!(patterns("off").is_empty());
    assert_eq!(patterns("rails"), ["Gemfile.lock"]);
    assert!(patterns("vite").is_empty());
}

#[test]
fn holds() {
    let servers = Arc::new(Servers::default());
    let wt = Path::new("/p/.claude/worktrees/x");
    let root = servers.hold(Path::new("/p"));
    let own = servers.hold(wt);
    assert!(servers.held(wt) && servers.held(Path::new("/p")));
    drop(root);
    assert!(servers.held(wt) && !servers.held(Path::new("/p")));
    let again = servers.hold(wt);
    drop(own);
    assert!(servers.held(wt));
    drop(again);
    assert!(!servers.held(wt) && servers.held.lock().is_empty());
}

#[test]
fn wildcards() {
    assert!(wildcard(b"*.exs", b"dev.exs"));
    assert!(wildcard(b"*", b""));
    assert!(wildcard(b"?ev.*s", b"dev.exs"));
    assert!(!wildcard(b"*.exs", b"dev.ex"));
    assert!(!wildcard(b"mix.lock", b"mix.lockx"));
}

#[test]
fn watched_files() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().canonicalize().unwrap();
    std::fs::create_dir(dir.join("config")).unwrap();
    for f in [
        "mix.exs",
        "mix.lock",
        "config/dev.exs",
        "config/notes.txt",
        "README.md",
    ] {
        std::fs::write(dir.join(f), "a").unwrap();
    }
    let patterns: Vec<String> = MIX_FILES.iter().map(|p| p.to_string()).collect();
    let w = Watched::new(dir.clone(), patterns.clone());
    assert_eq!(w.files.len(), 3);
    // Rewritten with the same content: nothing to restart for.
    std::fs::write(dir.join("mix.lock"), "a").unwrap();
    assert_eq!(hashes(&dir, &patterns), w.files);
    std::fs::write(dir.join("mix.lock"), "b").unwrap();
    assert_ne!(hashes(&dir, &patterns), w.files);

    assert!(w.concerns(&dir.join("mix.lock")));
    assert!(w.concerns(&dir.join("config/runtime.exs")));
    // Made after the watch began.
    assert!(w.concerns(&dir.join("config")));
    assert!(!w.concerns(&dir.join("lib/a.exs")));
}
