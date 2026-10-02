use super::*;

fn route(hostname: &str, port: u16, owner: &str) -> Route {
    Route {
        hostname: hostname.into(),
        upstream: SocketAddr::from(([127, 0, 0, 1], port)),
        owner: owner.into(),
        tls: None,
    }
}

#[test]
fn registration_is_normalized_and_idempotent() {
    let t = DevenvRoutes::default();
    t.register(route("WEB.Demo.Localhost.", 3000, "demo"))
        .unwrap();
    t.register(route("web.demo.localhost", 3001, "demo"))
        .unwrap();
    assert_eq!(
        t.resolve("WEB.demo.localhost"),
        Some(SocketAddr::from(([127, 0, 0, 1], 3001)))
    );
    assert_eq!(t.list().len(), 1);
}

#[test]
fn replacement_reconciles_only_the_owners_routes() {
    let t = DevenvRoutes::default();
    t.register(route("api.other.localhost", 9000, "other"))
        .unwrap();
    t.register(route("old.demo.localhost", 8000, "demo"))
        .unwrap();
    t.replace_owner("demo", vec![route("new.demo.localhost", 8001, "demo")])
        .unwrap();
    assert_eq!(t.resolve("old.demo.localhost"), None);
    assert!(t.resolve("new.demo.localhost").is_some());
    assert!(t.resolve("api.other.localhost").is_some());
}

#[test]
fn owners_cannot_take_each_others_routes() {
    let t = DevenvRoutes::default();
    t.register(route("web.demo.localhost", 3000, "demo"))
        .unwrap();
    assert!(
        t.register(route("web.demo.localhost", 4000, "other"))
            .is_err()
    );
    assert!(t.unregister("web.demo.localhost", "other").is_err());
    assert!(
        t.replace_owner("other", vec![route("web.demo.localhost", 4000, "other")])
            .is_err()
    );
    assert_eq!(
        t.resolve("web.demo.localhost").map(|a| a.port()),
        Some(3000)
    );
}

#[test]
fn lazy_cow_tree_hosts_and_non_local_routes_are_refused() {
    let t = DevenvRoutes::new(Arc::new(|h| h == "web.app.localhost"), None, false);
    let e = t
        .register(route("web.app.localhost", 3000, "x"))
        .unwrap_err();
    assert!(e.to_string().contains("served by lazy-cow-tree"), "{e}");
    assert!(t.register(route("example.com", 3000, "x")).is_err());
    let mut public = route("web.x.localhost", 3000, "x");
    public.upstream = SocketAddr::from(([192, 0, 2, 1], 3000));
    assert!(t.register(public).is_err());
}

#[test]
fn own_ca_ignores_the_projects_certificate() {
    let mut tls = route("web.x.localhost", 3000, "x");
    tls.tls = Some(TlsConfig {
        certificate: "/nonexistent/cert.pem".into(),
        key: "/nonexistent/key.pem".into(),
    });
    let project = DevenvRoutes::new(Arc::new(|_| false), None, false);
    assert!(project.register(tls.clone()).is_err());
    let own = DevenvRoutes::new(Arc::new(|_| false), None, true);
    own.register(tls).unwrap();
    assert!(own.certificate("web.x.localhost").is_none());
    assert_eq!(
        own.list()[0].tls.as_ref().map(|t| t.key.as_path()),
        Some(Path::new("/nonexistent/key.pem"))
    );
}

#[test]
fn wire_format_matches_devenv() {
    let req: ControlRequest = serde_json::from_str(
        r#"{"command":"replace_owner","owner":"demo","routes":[{"hostname":"web.demo.localhost","upstream":"127.0.0.1:3000","owner":"demo"}]}"#,
    )
    .unwrap();
    assert!(matches!(req, ControlRequest::ReplaceOwner { .. }));
    let status = ControlResponse::Info {
        pid: 1,
        https_listen: Some(SocketAddr::from(([127, 0, 0, 1], 443))),
    };
    assert_eq!(
        serde_json::to_string(&status).unwrap(),
        r#"{"status":"info","pid":1,"https_listen":"127.0.0.1:443"}"#
    );
    assert_eq!(
        serde_json::to_string(&ControlResponse::Ok {
            routes: None,
            removed: Some(true)
        })
        .unwrap(),
        r#"{"status":"ok","routes":null,"removed":true}"#
    );
}

#[tokio::test]
async fn serves_devenv_clients_over_a_private_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("proxy.sock");
    let routes = DevenvRoutes::default();
    let server = tokio::spawn({
        let (socket, routes) = (socket.clone(), routes.clone());
        async move { serve(&socket, routes).await }
    });
    let ask = |body: &'static str| {
        let socket = socket.clone();
        async move {
            let mut s = loop {
                if let Ok(s) = UnixStream::connect(&socket).await {
                    break s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            s.write_all(body.as_bytes()).await.unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).await.unwrap();
            line
        }
    };
    let reply = ask(
        "{\"command\":\"register\",\"route\":{\"hostname\":\"web.demo.localhost\",\"upstream\":\"127.0.0.1:3000\",\"owner\":\"demo\"}}\n",
    )
    .await;
    assert!(reply.starts_with(r#"{"status":"ok""#), "{reply}");
    assert!(routes.resolve("web.demo.localhost").is_some());
    let reply = ask("not json\n").await;
    assert!(reply.starts_with(r#"{"status":"error""#), "{reply}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A second daemon refuses the live socket instead of stealing it.
    assert!(serve(&socket, DevenvRoutes::default()).await.is_err());
    server.abort();
}

#[tokio::test]
async fn never_replaces_a_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("proxy.sock");
    std::fs::write(&socket, "keep me").unwrap();
    assert!(serve(&socket, DevenvRoutes::default()).await.is_err());
    assert_eq!(std::fs::read_to_string(socket).unwrap(), "keep me");
}
