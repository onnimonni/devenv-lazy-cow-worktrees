use super::*;

fn named(name: &str, env: &[(&str, &str)]) -> Project {
    let mut p = Project::new(
        format!("/src/{name}").into(),
        serde_json::from_value(serde_json::json!({
            "name": name, "port": 4000, "remote": "origin", "base": null,
            "worktrees_dir": ".claude/worktrees", "migrate": null, "seed": null,
            "setup": null, "services": {},
            "no_sync": false, "no_auto_remove": false
        }))
        .unwrap(),
    );
    p.env = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    p
}

#[test]
fn durability_must_match_the_daemons() {
    let fast = named("fast", &[]);
    let safe = named("safe", &[("LAZY_COW_TREE_POSTGRES_DURABLE", "1")]);
    assert!(durability_check(false, &fast, std::iter::empty()).is_ok());
    assert!(durability_check(true, &safe, std::iter::empty()).is_ok());
    let e = durability_check(false, &safe, [&fast].into_iter())
        .unwrap_err()
        .to_string();
    assert!(e.contains("project safe wants a durable PostgreSQL"), "{e}");
    assert!(e.contains("serving fast"), "{e}");
    assert!(durability_check(true, &fast, std::iter::empty()).is_err());
    // Its own earlier registration doesn't count as another project.
    let e = durability_check(false, &safe, [&safe].into_iter())
        .unwrap_err()
        .to_string();
    assert!(e.contains("the project that started the daemon"), "{e}");
}

#[test]
fn redis_key_per_checkout_or_shared() {
    let own = named("app", &[]);
    let shared = named("app", &[("LAZY_COW_TREE_REDIS_INSTANCE", "shared")]);
    let (p, w) = (
        own.checkout(None, "/src/app".into()),
        own.checkout_on(Some("wt"), "/src/app/wt".into(), 20000),
    );
    assert_eq!(redis_key(&own, &p), p.id());
    assert_eq!(redis_key(&own, &w), w.id());
    assert_ne!(redis_key(&own, &p), redis_key(&own, &w));
    assert_eq!(redis_key(&shared, &p), redis_key(&shared, &w));
}

fn failed(attempts: u32, ago: Duration) -> MigrateFailure {
    MigrateFailure {
        error: String::new(),
        attempts,
        at: std::time::Instant::now().checked_sub(ago).unwrap(),
    }
}

fn co(wt: Option<&str>) -> Checkout {
    Checkout {
        project: "demo".into(),
        db_prefix: "demo".into(),
        worktree: wt.map(str::to_string),
        path: "/x".into(),
        port: 20000,
        services: Default::default(),
    }
}

#[test]
fn migrate_backoff_doubles_and_caps() {
    assert!(failed(1, Duration::from_secs(30)).backing_off());
    assert!(!failed(1, Duration::from_secs(61)).backing_off());
    assert!(failed(2, Duration::from_secs(100)).backing_off());
    assert!(!failed(2, Duration::from_secs(121)).backing_off());
    // Capped at 32 minutes.
    assert!(failed(50, Duration::from_secs(31 * 60)).backing_off());
    assert!(!failed(50, Duration::from_secs(33 * 60)).backing_off());
}

#[test]
fn primary_setup_runs_until_it_succeeds_with_backoff() {
    use SetupStep::*;
    // Marker present or no setup command: nothing to do, whatever failed.
    assert_eq!(primary_setup_step(false, None, false), Done);
    let recent = failed(1, Duration::from_secs(10));
    assert_eq!(primary_setup_step(false, Some(&recent), false), Done);
    // Pending: runs, unless its last failure still backs off (forced anyway).
    assert_eq!(primary_setup_step(true, None, false), Run);
    assert_eq!(primary_setup_step(true, Some(&recent), false), BackingOff);
    assert_eq!(primary_setup_step(true, Some(&recent), true), Run);
    let old = failed(1, Duration::from_secs(61));
    assert_eq!(primary_setup_step(true, Some(&old), false), Run);
}

#[test]
fn setup_marker_in_git_admin_dir() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let marker = setup_marker(dir.path()).unwrap();
    assert_eq!(marker, repo.path().join("lazy-cow-tree-setup"));
    assert!(marker.parent().unwrap().ends_with(".git"));
    assert!(setup_marker(&dir.path().join("missing")).is_err());
}

#[test]
fn pg_access_fails_closed() {
    let wt = co(Some("wt"));
    // Not a registered checkout's role: refused, whatever the database.
    for db in ["postgres", "demo_dev", "anything"] {
        let e = pg_access("stranger", None, db, &[]).unwrap_err();
        assert!(e.to_string().contains("not the role of a checkout"), "{e}");
    }
    assert!(pg_access("postgres", None, "postgres", &[]).is_err());
    assert!(pg_access("postgres", Some(&co(None)), "postgres", &[]).is_err());
    // Registered: maintenance and own databases, the worktree's dev one created.
    assert!(!pg_access("demo--wt", Some(&wt), "postgres", &[]).unwrap());
    assert!(!pg_access("demo--wt", Some(&wt), "template1", &[]).unwrap());
    assert!(pg_access("demo--wt", Some(&wt), "demo_dev_wt", &[]).unwrap());
    assert!(!pg_access("demo--wt", Some(&wt), "demo_test_wt", &[]).unwrap());
    assert!(!pg_access("demo", Some(&co(None)), "demo_dev", &[]).unwrap());
    assert!(pg_access("demo--wt", Some(&wt), "demo_dev", &[]).is_err());
    // Partitions: a registered worktree's own name wins; others (and itself) listed.
    let (x, x2) = (co(Some("x")), co(Some("x2")));
    let all = [x.clone(), x2.clone()];
    assert!(pg_access("demo--x", Some(&x), "demo_test_x3", &all).is_ok());
    let e = pg_access("demo--x", Some(&x), "demo_test_x2", &all).unwrap_err();
    assert!(e.to_string().contains("demo--x2"), "{e}");
    assert!(pg_access("demo--x2", Some(&x2), "demo_test_x2", &all).is_ok());
    // Cross-project tie: nobody may open it.
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
    assert_eq!(a.dev_db(), b.dev_db());
    let both = [a.clone(), b.clone()];
    assert!(pg_access("shop-dev-x", Some(&a), "shop_dev_dev_x", &both).is_err());
    assert!(pg_access("shop-dev-x", Some(&b), "shop_dev_dev_x", &both).is_err());
}
