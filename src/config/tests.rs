use super::*;

fn co(wt: Option<&str>) -> Checkout {
    Checkout {
        project: "my-app".into(),
        db_prefix: "my_app".into(),
        worktree: wt.map(str::to_string),
        path: "/x".into(),
        port: 20000,
        services: Services::default(),
        extra_dbs: Vec::new(),
    }
}

fn with_services(json: &str) -> Checkout {
    Checkout {
        services: json.parse().unwrap(),
        ..co(Some("wt"))
    }
}

#[test]
fn service_start_ready_hostname_and_back_compat() {
    // Without the new fields: as before.
    let old: Services = r#"{"web": {"exec": "mix phx.server"}}"#.parse().unwrap();
    let web = &old.0["web"];
    assert_eq!(web.start, StartMode::Demand);
    assert_eq!(
        (web.idle_timeout, web.ready.as_ref(), web.hostname.as_ref()),
        (None, None, None)
    );
    let new: Services = r#"{"web": {"exec": "/nix/store/x-web", "start": "up", "idleTimeout": 900,
        "ready": {"timeout": 5}, "hostname": "care.treat.localhost"},
        "adm": {"exec": "x", "start": "manual"}}"#
        .parse()
        .unwrap();
    let web = &new.0["web"];
    assert_eq!(web.start, StartMode::Up);
    assert_eq!(new.0["adm"].start, StartMode::Manual);
    assert_eq!(web.idle_timeout, Some(900));
    assert_eq!(
        web.ready,
        Some(Ready {
            path: "/".into(),
            timeout: 5
        })
    );
    // Unknown fields are still refused.
    assert!(r#"{"web": {"exec": "x", "nope": 1}}"#.parse::<Services>().is_err());
    assert!(r#"{"web": {"exec": "x", "start": "later"}}"#.parse::<Services>().is_err());
}

#[test]
fn service_hostname_override() {
    let c = with_services(
        r#"{"care": {"exec": "x", "hostname": "care.treat.localhost"}, "web": {"exec": "y"}}"#,
    );
    assert_eq!(c.service_host("care"), "wt.care.treat.localhost");
    assert_eq!(c.service_host("web"), "wt.web.my-app.localhost");
    let primary = Checkout {
        worktree: None,
        ..c.clone()
    };
    assert_eq!(primary.service_host("care"), "care.treat.localhost");
    assert!(
        c.routes()
            .iter()
            .any(|(_, h, _)| h == "wt.care.treat.localhost")
    );
    for bad in [
        r#"{"a": {"exec": "x", "hostname": "Care.localhost"}}"#,
        r#"{"a": {"exec": "x", "hostname": "care.example.com"}}"#,
        r#"{"a": {"exec": "x", "hostname": "-a.localhost"}}"#,
        r#"{"a": {"exec": "x", "hostname": "h.localhost"}, "b": {"exec": "y", "hostname": "h.localhost"}}"#,
    ] {
        assert!(bad.parse::<Services>().is_err(), "{bad}");
    }
}

#[test]
fn rewrites_primary_paths_to_the_worktree() {
    let (r, w) = (
        Path::new("/src/app"),
        Path::new("/src/app/.claude/worktrees/wt"),
    );
    assert_eq!(
        rewrite_root("/src/app", r, w),
        "/src/app/.claude/worktrees/wt"
    );
    assert_eq!(
        rewrite_root("/src/app/bin:/usr/bin:/src/app/node_modules/.bin", r, w),
        "/src/app/.claude/worktrees/wt/bin:/usr/bin:/src/app/.claude/worktrees/wt/node_modules/.bin"
    );
    // Already the worktree's, another directory, or mid-word: untouched.
    for keep in [
        "/src/app/.claude/worktrees/wt/x",
        "/src/application",
        "x/src/app",
    ] {
        assert_eq!(rewrite_root(keep, r, w), keep);
    }
    assert_eq!(
        rewrite_root("cd \"/src/app/api\" && exec x", r, Path::new("/wt")),
        "cd \"/wt/api\" && exec x"
    );
    assert_eq!(rewrite_root("/src/app/x", r, r), "/src/app/x");
}

#[test]
fn project_settings_from_env() {
    let settings = ProjectSettings {
        name: Some("app".into()),
        port: 4000,
        remote: "origin".into(),
        base: None,
        worktrees_dir: ".claude/worktrees".into(),
        migrate: None,
        seed: None,
        setup: None,
        services: Default::default(),
        no_sync: false,
        no_auto_remove: false,
        databases: Vec::new(),
    };
    let mut p = Project::new("/src/app".into(), settings);
    assert!(p.copy_on_write());
    assert!(!p.template_refresh_manual() && !p.postgres_durable());
    assert!(!p.redis_shared() && !p.redis_start_up());
    p.env = [
        ("LAZY_COW_TREE_POSTGRES_COW", "0"),
        ("LAZY_COW_TREE_POSTGRES_TEMPLATE_REFRESH", "manual"),
        ("LAZY_COW_TREE_POSTGRES_DURABLE", "1"),
        ("LAZY_COW_TREE_REDIS_INSTANCE", "shared"),
        ("LAZY_COW_TREE_REDIS_START", "up"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .to_vec();
    assert!(!p.copy_on_write());
    assert!(p.template_refresh_manual() && p.postgres_durable());
    assert!(p.redis_shared() && p.redis_start_up());
}

#[test]
fn services() {
    let c = with_services(
        r#"{"web": {"exec": "mix phx.server", "dependsOn": ["worker"]},
            "api": {"exec": "bun dev", "cwd": "api"},
            "worker": {"exec": "mix run", "http": false}}"#,
    );
    assert_eq!(c.services.default_name(), Some("web"));
    // Offsets by name: api 0, web 1, worker 2.
    assert_eq!(c.service_port("api"), 20000);
    assert_eq!(c.service_port("web"), 20001);
    assert_eq!(c.service_host("web"), "wt.web.my-app.localhost");
    assert_eq!(c.service_host("api"), "wt.api.my-app.localhost");
    assert_eq!(
        c.routes(),
        vec![
            (Some("api".into()), "wt.api.my-app.localhost".into(), 20000),
            (Some("web".into()), "wt.web.my-app.localhost".into(), 20001),
        ]
    );
    assert_eq!(c.main_host(), "wt.web.my-app.localhost");
    let primary = Checkout {
        worktree: None,
        ..c.clone()
    };
    assert_eq!(primary.service_host("api"), "api.my-app.localhost");

    // Every service shares the checkout's database and Redis (for now).
    let env: BTreeMap<_, _> = c.service_env(&global(), Some("api")).into_iter().collect();
    assert_eq!(env["PORT"], "20000");
    assert_eq!(env["LAZY_COW_TREE_URL"], "https://wt.api.my-app.localhost");
    assert_eq!(env["PGDATABASE"], "my_app_dev_wt");
    assert!(env["REDIS_URL"].starts_with("redis://:my-app--wt@"));
    assert_eq!(
        env["LAZY_COW_TREE_WEB_URL"],
        "https://wt.web.my-app.localhost"
    );
    assert_eq!(env["LAZY_COW_TREE_WORKER_PORT"], "20002");
    assert!(!env.contains_key("LAZY_COW_TREE_WORKER_URL"));
    let web: BTreeMap<_, _> = c.env(&global()).into_iter().collect();
    assert_eq!(web["PORT"], "20001");
    assert_eq!(web["DATABASE_URL"], env["DATABASE_URL"]);
    assert_eq!(web["REDIS_URL"], env["REDIS_URL"]);

    assert!(
        "{\"a\": {\"exec\": \"x\", \"dependsOn\": [\"b\"]}}"
            .parse::<Services>()
            .is_err()
    );
    assert!(
        "{\"a\": {\"exec\": \"x\", \"portOffset\": 10}}"
            .parse::<Services>()
            .is_err()
    );
    assert!(
        "{\"a\": {\"exec\": \"x\", \"bogus\": 1}}"
            .parse::<Services>()
            .is_err()
    );
}

#[test]
fn named_ports() {
    let mut c = with_services(
        r#"{"web": {"exec": "mix phx.server", "ports": {
                "debugger": {"env": "LIVE_DEBUGGER_PORT", "http": true},
                "test": {"env": "TEST_PORT"}}},
            "worker": {"exec": "mix run", "http": false,
                "ports": {"metrics": {"offset": 5}, "admin": {}}}}"#,
    );
    c.worktree = None;
    c.port = 4000;
    // Services by name (web 0, worker 1); explicit offsets; the rest top down by
    // service then port name: web.debugger 9, web.test 8, worker.admin 7.
    let slots: Vec<_> = c
        .port_slots()
        .into_iter()
        .map(|p| (p.service, p.name, p.offset, p.env))
        .collect();
    assert_eq!(
        slots,
        [
            (
                "web".into(),
                "debugger".into(),
                9,
                "LIVE_DEBUGGER_PORT".into()
            ),
            ("web".into(), "test".into(), 8, "TEST_PORT".into()),
            ("worker".into(), "admin".into(), 7, "ADMIN_PORT".into()),
            ("worker".into(), "metrics".into(), 5, "METRICS_PORT".into()),
        ]
    );
    assert_eq!(c.service_port("web"), 4000);
    assert_eq!(c.service_port("worker"), 4001);
    assert_eq!(
        c.routes(),
        vec![
            (Some("web".into()), "web.my-app.localhost".into(), 4000),
            (Some("web".into()), "debugger.my-app.localhost".into(), 4009),
        ]
    );
    let wt = Checkout {
        worktree: Some("wt".into()),
        ..c.clone()
    };
    assert!(wt.routes().contains(&(
        Some("web".into()),
        "wt.debugger.my-app.localhost".into(),
        4009
    )));
    // In every environment of the checkout.
    for svc in [None, Some("web"), Some("worker")] {
        let env: BTreeMap<_, _> = c.service_env(&global(), svc).into_iter().collect();
        assert_eq!(env["LIVE_DEBUGGER_PORT"], "4009");
        assert_eq!(env["TEST_PORT"], "4008");
        assert_eq!(env["ADMIN_PORT"], "4007");
        assert_eq!(env["METRICS_PORT"], "4005");
        assert_eq!(env["LAZY_COW_TREE_WEB_DEBUGGER_PORT"], "4009");
        assert_eq!(
            env["LAZY_COW_TREE_WEB_DEBUGGER_URL"],
            "https://debugger.my-app.localhost"
        );
        assert_eq!(env["LAZY_COW_TREE_WEB_TEST_PORT"], "4008");
        assert!(!env.contains_key("LAZY_COW_TREE_WEB_TEST_URL"));
    }

    let err = |json: &str| json.parse::<Services>().unwrap_err();
    // Full block: 1 service + 10 ports.
    let ports: Vec<String> = (0..10).map(|i| format!("\"p{i}\": {{}}")).collect();
    let full = format!(
        r#"{{"web": {{"exec": "x", "ports": {{{}}}}}}}"#,
        ports.join(",")
    );
    assert!(err(&full).contains("block is full"), "{}", err(&full));
    assert!(
        err(r#"{"web": {"exec": "x", "ports": {"a": {"offset": 0}}}}"#)
            .contains("share port offset 0")
    );
    assert!(err(r#"{"web": {"exec": "x", "ports": {"a": {"offset": 10}}}}"#).contains("over 9"));
    assert!(
        err(r#"{"web": {"exec": "x", "ports": {"api": {"http": true}}}, "api": {"exec": "y"}}"#)
            .contains("service api's")
    );
    assert!(
        err(r#"{"a": {"exec": "x", "ports": {"d": {"http": true}}}, "b": {"exec": "y", "ports": {"d": {"http": true, "env": "D2"}}}}"#)
            .contains("http port d")
    );
    assert!(
        err(r#"{"a": {"exec": "x", "ports": {"p": {}}}, "b": {"exec": "y", "ports": {"q": {"env": "P_PORT"}}}}"#)
            .contains("share env P_PORT")
    );
    assert!(
        err(r#"{"a": {"exec": "x", "ports": {"p": {"env": "lower"}}}}"#).contains("not [A-Z_]")
    );
    assert!(err(r#"{"a": {"exec": "x", "ports": {"P": {}}}}"#).contains("a-z"));
    for reserved in RESERVED_ENV.iter().chain(&["LAZY_COW_TREE_X"]) {
        let json =
            format!(r#"{{"a": {{"exec": "x", "ports": {{"p": {{"env": "{reserved}"}}}}}}}}"#);
        assert!(err(&json).contains("set by lazy-cow-tree"), "{reserved}");
    }
    assert!(
        err(r#"{"web": {"exec": "x", "ports": {"debugger": {}}}, "web-debugger": {"exec": "y"}}"#)
            .contains("both set LAZY_COW_TREE_WEB_DEBUGGER_PORT")
    );
    // Everything else service_env sets is reserved.
    let env = c.service_env(&global(), Some("web"));
    let ports: Vec<String> = c.port_slots().into_iter().map(|p| p.env).collect();
    for (k, _) in env {
        assert!(
            k.starts_with("LAZY_COW_TREE_")
                || RESERVED_ENV.contains(&k.as_str())
                || ports.contains(&k),
            "{k} not in RESERVED_ENV"
        );
    }
    // Null options, as the devenv module's JSON has them.
    assert!(
        r#"{"a": {"exec": "x", "ports": {"p": {"env": null, "http": false, "offset": null}}}}"#
            .parse::<Services>()
            .is_ok()
    );
}

#[test]
fn detects_frameworks() {
    let dir = tempfile::TempDir::new().unwrap();
    let d = dir.path();
    let keys = |d: &Path| -> Vec<String> {
        framework_env(d, "h.localhost")
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    };
    assert!(keys(d).is_empty());
    // Umbrella: Phoenix only in an app.
    std::fs::create_dir_all(d.join("apps/web")).unwrap();
    std::fs::write(d.join("mix.exs"), "defmodule U.MixProject do end").unwrap();
    std::fs::write(
        d.join("apps/web/mix.exs"),
        r#"defp deps, do: [{:phoenix, "~> 1.8"}]"#,
    )
    .unwrap();
    assert_eq!(keys(d), ["PHX_HOST"]);
    std::fs::write(d.join("Gemfile"), "gem \"rails\", \"~> 8.0\"").unwrap();
    std::fs::write(
        d.join("package.json"),
        r#"{"devDependencies": {"vite": "^7"}}"#,
    )
    .unwrap();
    assert!(keys(d).iter().all(|k| RESERVED_ENV.contains(&k.as_str())));
    assert_eq!(
        keys(d),
        [
            "PHX_HOST",
            "RAILS_DEVELOPMENT_HOSTS",
            "__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS"
        ]
    );
    // phoenix_live_view alone isn't Phoenix.
    let other = tempfile::TempDir::new().unwrap();
    std::fs::write(
        other.path().join("mix.exs"),
        r#"[{:phoenix_live_view, "~> 1.0"}]"#,
    )
    .unwrap();
    assert!(keys(other.path()).is_empty());
}

#[test]
fn postgres_extensions() {
    assert_eq!(
        global().postgres_extensions(),
        ["postgis", "vector", "pg_trgm"]
    );
    let none = Global {
        postgres_extensions: None,
        ..global()
    };
    assert!(none.postgres_extensions().is_empty());
}

#[test]
fn postgres_settings() {
    assert_eq!(
        global().postgres_settings().unwrap(),
        vec![
            ("jit".to_string(), "off".to_string()),
            ("n".into(), "3".into()),
            ("shared_preload_libraries".into(), "x".into()),
        ]
    );
}

fn global() -> Global {
    Global {
        pg_port: 55432,
        redis_port: 6380,
        https_port: 443,
        http_port: 80,
        ramdisk_mb: 1,
        postgres_bin: None,
        postgres_settings: Some(
            r#"{"shared_preload_libraries": "x", "jit": false, "n": 3}"#.into(),
        ),
        redis_server: None,
        postgres_extensions: Some("postgis, vector  pg_trgm,".into()),
        devenv_proxy_socket: None,
        postgres_durable: false,
        redis_idle_timeout: None,
    }
}

#[test]
fn state_dir_moves_from_the_former_name_once() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(state_dir(d.path()), d.path().join("lazy-cow-tree"));
    std::fs::create_dir_all(d.path().join("localforest/ca")).unwrap();
    std::fs::write(d.path().join("localforest/ca/ca.pem"), "x").unwrap();
    let dir = state_dir(d.path());
    assert_eq!(dir, d.path().join("lazy-cow-tree"));
    assert!(dir.join("ca/ca.pem").exists());
    assert!(!d.path().join("localforest").exists());
    // Both present: the new one wins, the old one is left alone.
    std::fs::create_dir(d.path().join("localforest")).unwrap();
    assert_eq!(state_dir(d.path()), dir);
    assert!(d.path().join("localforest").exists());
}

#[test]
fn names() {
    assert_eq!(dns_label("Fix Login_Bug!"), "fix-login-bug");
    assert_eq!(worktree_label("fix-login-bug"), "fix-login-bug");
    // Names that aren't labels get a hash, so they can't meet another's label.
    let a = worktree_label("Fix Login_Bug!");
    assert!(valid_label(&a) && a.starts_with("fix-login-bug-"), "{a}");
    assert_ne!(a, worktree_label("fix login bug"));
    let long = "a-very-long-task-name-that-goes-past-32-characters";
    let x = worktree_label(&format!("{long}-one"));
    let y = worktree_label(&format!("{long}-two"));
    assert!(valid_label(&x) && valid_label(&y) && x != y, "{x} {y}");
    assert_eq!(x, worktree_label(&format!("{long}-one")));
    assert!(valid_label(&worktree_label("---")));
    assert_eq!(co(None).host(), "my-app.localhost");
    assert_eq!(co(Some("fix-it")).host(), "fix-it.my-app.localhost");
    assert_eq!(co(Some("fix-it")).dev_db(), "my_app_dev_fix_it");
    assert_eq!(co(None).test_db(), "my_app_test");
    // Project `shop` + worktree `admin-foo` vs project `shop-admin` + worktree `foo`.
    let id = |p: &str, w: Option<&str>| {
        Checkout {
            project: p.into(),
            ..co(w)
        }
        .id()
    };
    assert_eq!(id("shop", Some("admin-foo")), "shop--admin-foo");
    assert_ne!(id("shop", Some("admin-foo")), id("shop-admin", Some("foo")));
    assert_ne!(id("shop-foo", None), id("shop", Some("foo")));
    assert_ne!(co(Some("x")).run_id("setup"), co(Some("x-setup")).id());
    // Fits PostgreSQL's 63-byte role names, still unique.
    let (a, b) = ("a".repeat(32), "b".repeat(31));
    let long = id(&a, Some(&format!("{b}1")));
    assert_eq!(long.len(), 63);
    assert_ne!(long, id(&a, Some(&format!("{b}2"))));
    // Old role names, for the upgrade.
    let wt = Checkout {
        path: "/r/wt".into(),
        ..co(Some("wt"))
    };
    assert_eq!(wt.legacy_id().as_deref(), Some("my-app-wt"));
    // Renamed by the git-admin-dir naming too: its pre-upgrade role.
    let renamed = Checkout {
        path: "/r/.claude/worktrees/Fix_It".into(),
        ..co(Some(&worktree_label("Fix_It")))
    };
    assert_eq!(renamed.legacy_id().as_deref(), Some("my-app-fix-it"));
    assert_eq!(co(None).legacy_id(), None);
    let p = worktree_port("a", "b");
    assert!((20000..29000).contains(&p) && p.is_multiple_of(10));
}

#[test]
fn secret_created_once_under_races() {
    for _ in 0..20 {
        let dir = tempfile::TempDir::new().unwrap();
        let secrets: Vec<Vec<u8>> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|_| sc.spawn(|| secret_in(dir.path()).unwrap()))
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(secrets.iter().all(|s| s.len() == 32 && *s == secrets[0]));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn database_names_fit_postgres() {
    let long = |p: &str, w: &str| Checkout {
        project: p.into(),
        db_prefix: p.replace('-', "_"),
        ..co(Some(w))
    };
    let p = "a-project-name-of-thirty-two-chr";
    assert_eq!(p.len(), 32);
    let w1 = format!("{}-1", "w".repeat(30));
    let w2 = format!("{}-2", "w".repeat(30));
    let (a, b) = (long(p, &w1), long(p, &w2));
    for c in [&a, &b] {
        for db in [c.dev_db(), c.test_db(), c.id()] {
            assert!(db.len() <= PG_NAME_MAX, "{db}");
        }
        // Partitions fit too, so PostgreSQL never truncates them.
        for part in [
            format!("{}9999", c.test_db()),
            format!("{}_test9999{}", c.db_prefix, c.suffix()),
        ] {
            assert!(part.len() <= PG_NAME_MAX, "{part}");
            assert!(c.owns_db(&part), "{part}");
        }
        assert!(c.owns_db(&c.dev_db()));
    }
    assert_ne!(a.dev_db(), b.dev_db());
    assert_ne!(a.test_db(), b.test_db());
    assert_ne!(a.id(), b.id());
    assert!(!a.owns_db(&b.test_db()));
    assert!(!a.owns_db(&format!("{}_test3{}", b.db_prefix, b.suffix())));
    assert!(!a.owns_db(&format!("{}3", b.test_db())));
    // Short names are unchanged.
    assert_eq!(co(Some("wt")).dev_db(), "my_app_dev_wt");
    assert_eq!(pg_name(&"x".repeat(63)), "x".repeat(63));
    assert_eq!(pg_name(&"x".repeat(64)).len(), 63);
    assert_ne!(pg_name(&"x".repeat(64)), pg_name(&"x".repeat(65)));
}

#[test]
fn legacy_names() {
    let c = Checkout {
        path: "/r/.claude/worktrees/Fix_It".into(),
        ..co(Some(&worktree_label("Fix_It")))
    };
    let old = c.legacy().unwrap();
    assert_eq!(old.name, "fix-it");
    assert_eq!(old.role, "my-app-fix-it");
    assert_eq!(old.dbs[0], ("my_app_dev_fix_it".into(), c.dev_db()));
    assert!(old.owns_partition("my_app_test2_fix_it"));
    assert!(!old.owns_partition("my_app_test_fix_it"));
    let same = Checkout {
        path: "/r/wt".into(),
        ..co(Some("wt"))
    };
    assert!(same.legacy().is_none());
}

#[test]
fn owned_databases() {
    let c = co(Some("wt"));
    assert!(c.owns_db("my_app_dev_wt"));
    assert!(c.owns_db("my_app_test_wt"));
    // MIX_TEST_PARTITION: appended to the test database, or `_test<N>_<wt>`.
    assert!(c.owns_db("my_app_test_wt2"));
    assert!(c.owns_db("my_app_test_wt12"));
    assert!(c.owns_db("my_app_test2_wt"));
    assert!(!c.owns_db("my_app_test_wt_2"));
    assert!(!c.owns_db("my_app_test_other_wt"));
    assert!(!c.owns_db("my_app_dev"));
    assert!(!c.owns_db("my_app_dev_wt2"));
    let p = co(None);
    assert!(p.owns_db("my_app_dev") && p.owns_db("my_app_test"));
    assert!(p.owns_db("my_app_test2"));
    assert!(!p.owns_db("my_app_dev_wt"));
    assert!(!p.owns_db("my_app_test_"));
}

#[test]
fn database_names_never_collide() {
    // Old forms that were other worktrees' test databases.
    assert!(!co(Some("x")).owns_db(&co(Some("1-x")).test_db()));
    assert!(!co(Some("x")).owns_db(&co(Some("p1-x")).test_db()));
    assert!(!co(None).owns_db(&co(Some("2")).test_db()));
    assert!(!co(None).owns_db(&co(Some("p2")).test_db()));
    // Nobody else's dev or test database is a `_test<N>_<wt>` partition.
    let names = ["x", "1-x", "p1-x", "2", "p2", "x2", "test2-x", "dev"];
    for a in names.iter().map(|n| co(Some(n))).chain([co(None)]) {
        for b in names.iter().map(|n| co(Some(n))).chain([co(None)]) {
            if a != b {
                for db in [b.dev_db(), b.test_db()] {
                    let exact_overlap = db.strip_prefix(&a.test_db()).is_some();
                    assert!(!a.owns_db(&db) || exact_overlap, "{a:?} owns {db}");
                }
            }
        }
    }
}

#[test]
fn partition_owner() {
    let (x, x2, x22) = (co(Some("x")), co(Some("x2")), co(Some("x22")));
    let all = [x.clone(), x2.clone(), x22.clone()];
    let owner = |db: &str, cs: &[Checkout]| db_owner(db, cs).map(|c| c.worktree.clone().unwrap());
    // x's partition 2 is worktree x2's test database while x2 exists.
    assert_eq!(owner("my_app_test_x2", &all).as_deref(), Some("x2"));
    assert_eq!(owner("my_app_test_x2", &all[..1]).as_deref(), Some("x"));
    // Longest test database wins among partitions.
    assert_eq!(owner("my_app_test_x23", &all).as_deref(), Some("x2"));
    assert_eq!(owner("my_app_test_x22", &all).as_deref(), Some("x22"));
    assert_eq!(owner("my_app_test_x3", &all).as_deref(), Some("x"));
    assert_eq!(owner("my_app_test2_x", &all).as_deref(), Some("x"));
    assert_eq!(owner("my_app_test_other", &all), None);
    let p = [co(None), co(Some("2"))];
    assert_eq!(owner("my_app_test_2", &p).as_deref(), Some("2"));
    assert_eq!(db_owner("my_app_test2", &p), Some(&p[0]));
    // The same checkout twice (registered, and the caller's copy) is no tie.
    assert_eq!(
        owner("my_app_test_x", &[x.clone(), x.clone()]).as_deref(),
        Some("x")
    );
    // Different projects' equal names: a tie, nobody owns it.
    let a = Checkout {
        project: "shop".into(),
        db_prefix: "shop".into(),
        ..co(Some("dev-x"))
    };
    let b = Checkout {
        project: "shop-dev".into(),
        db_prefix: "shop_dev".into(),
        ..co(Some("x"))
    };
    assert_eq!(db_owner("shop_dev_dev_x", [&a, &b]), None);
    assert_eq!(db_owner("shop_dev_dev_x", [&a]), Some(&a));
}

#[test]
fn extra_databases_per_checkout() {
    let with = |wt: Option<&str>| Checkout {
        extra_dbs: vec!["cms".into()],
        ..co(wt)
    };
    let (primary, wt) = (with(None), with(Some("feat-x")));
    assert_eq!(primary.dev_dbs(), ["my_app_dev", "my_app_cms_dev"]);
    assert_eq!(wt.dev_dbs(), ["my_app_dev_feat_x", "my_app_cms_dev_feat_x"]);
    assert_eq!(wt.template_db_of(Some("cms")), "my_app_cms_template");
    // Test databases and MIX_TEST_PARTITION ones of each kind are its own.
    for db in [
        "my_app_cms_test_feat_x",
        "my_app_cms_test_feat_x3",
        "my_app_cms_test2_feat_x",
        "my_app_test_feat_x3",
    ] {
        assert!(wt.owns_db(db), "{db}");
        assert!(!primary.owns_db(db), "{db}");
    }
    assert!(primary.owns_db("my_app_cms_test4"));
    assert!(!wt.owns_db("my_app_cms_dev") && !co(Some("feat-x")).owns_db("my_app_cms_dev_feat_x"));
    // An exact name beats a partition, as for the main databases.
    let x2 = with(Some("x2"));
    assert_eq!(
        db_owner("my_app_cms_test_x2", [&with(Some("x")), &x2]).map(|c| c.id()),
        Some(x2.id())
    );
    let env: BTreeMap<_, _> = wt.env(&global()).into_iter().collect();
    assert!(env["CMS_DATABASE_URL"].ends_with("/my_app_cms_dev_feat_x"));
    assert!(env["CMS_TEST_DATABASE_URL"].ends_with("/my_app_cms_test_feat_x"));
    assert!(env["DATABASE_URL"].ends_with("/my_app_dev_feat_x"));
    // Names fit PostgreSQL's 63 bytes with the extra name's length counted.
    let long = with(Some(&"w".repeat(60)));
    for kind in long.db_kinds() {
        assert!(long.test_db_of(kind).len() + "_p9999".len() <= PG_NAME_MAX);
    }
    assert!(
        parse_db_name("cms").is_ok() && parse_db_name("Cms").is_err() && parse_db_name("").is_err()
    );
}
