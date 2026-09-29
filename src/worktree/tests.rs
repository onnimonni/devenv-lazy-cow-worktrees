use super::procs::{sweep, with_ancestors};
use super::*;
use crate::config::ProjectSettings;
use git2::{Signature, WorktreeAddOptions};
use tempfile::TempDir;

fn settings() -> ProjectSettings {
    ProjectSettings {
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
    }
}

fn fixture() -> (TempDir, Project, Syncer) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("app");
    let repo = Repository::init(&root).unwrap();
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    std::fs::write(root.join(".gitignore"), "cache/\n").unwrap();
    std::fs::create_dir(root.join("cache")).unwrap();
    std::fs::write(root.join("cache/big"), "built\n").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new("a.txt")).unwrap();
    idx.add_path(Path::new(".gitignore")).unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = Signature::now("T", "t@example.com").unwrap();
    repo.commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    let root = root.canonicalize().unwrap();
    let project = Project::new(root.clone(), settings());
    let syncer = Syncer {
        path: root,
        remote: "origin".into(),
        base: "main".into(),
        token: None,
        token_host: "github.com".into(),
    };
    (dir, project, syncer)
}

#[test]
fn names_are_unique() {
    let (d, project, syncer) = fixture();
    // Long requested names that share their first 32 characters get their own.
    let long = "implement-the-very-long-feature-name";
    let a = create(
        &project,
        &syncer,
        &worktree_label(&format!("{long} one")),
        None,
    )
    .unwrap();
    let b = create(
        &project,
        &syncer,
        &worktree_label(&format!("{long} two")),
        None,
    )
    .unwrap();
    assert_ne!(a, b);

    // Same directory name elsewhere (plain `git worktree add`): git numbers the
    // admin dir, and list and the shell hook agree on the name.
    let repo = Repository::open(&project.root).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    for (admin, dir) in [("dup", "x"), ("dup1", "y")] {
        let branch = repo.branch(admin, &head, false).unwrap();
        let r = branch.into_reference();
        std::fs::create_dir_all(d.path().join(dir)).unwrap();
        repo.worktree(
            admin,
            &d.path().join(dir).join("dup"),
            Some(WorktreeAddOptions::new().reference(Some(&r))),
        )
        .unwrap();
    }
    let infos = list(&project.root).unwrap();
    let mut names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
    names.sort();
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(names.contains(&"dup") && names.contains(&"dup1"));
    for i in &infos {
        let (_, name, _) = crate::config::locate(&i.path).unwrap();
        assert_eq!(name.as_deref(), Some(i.name.as_str()));
    }

    // A directory holding another worktree is never handed out under a new name.
    std::fs::create_dir_all(project.worktrees_dir()).unwrap();
    let taken = project.worktrees_dir().join("other");
    let r = repo
        .branch("other-branch", &head, false)
        .unwrap()
        .into_reference();
    repo.worktree(
        "other-admin",
        &taken,
        Some(WorktreeAddOptions::new().reference(Some(&r))),
    )
    .unwrap();
    let err = create(&project, &syncer, "other", None).unwrap_err();
    assert!(err.to_string().contains("is worktree other-admin"), "{err}");
    // A name git gave another worktree's admin dir.
    let err = create(&project, &syncer, "dup1", None).unwrap_err();
    assert!(err.to_string().contains("pick another name"), "{err}");
}

#[test]
fn ports_never_collide() {
    let port_of = |plan: &[(PathBuf, u16, bool)], p: &Path| {
        plan.iter()
            .find(|(q, ..)| q == p)
            .map(|(_, port, _)| *port)
            .unwrap()
    };
    let (_d, project, syncer) = fixture();
    let a = create(&project, &syncer, "a", None).unwrap();
    let b = create(&project, &syncer, "b", None).unwrap();
    // Uncontested: the hashed slot, recorded by assign_ports only.
    let plan = plan_ports(std::slice::from_ref(&project)).unwrap();
    assert_eq!(port_of(&plan, &b), config::worktree_port("app", "b"));
    assert_eq!(recorded_port(&b), None);
    assign_ports(std::slice::from_ref(&project)).unwrap();
    assert_eq!(recorded_port(&b), Some(config::worktree_port("app", "b")));

    // a holds c's slot (a hash collision): c moves on, and env's plan agrees.
    let slot_c = config::worktree_port("app", "c");
    std::fs::write(
        admin_dir(&a).unwrap().join(PORT_FILE),
        format!("{slot_c}\n"),
    )
    .unwrap();
    let c = create(&project, &syncer, "c", None).unwrap();
    let planned = port_of(&plan_ports(std::slice::from_ref(&project)).unwrap(), &c);
    let assigned = assign_ports(std::slice::from_ref(&project)).unwrap();
    let pc = assigned.iter().find(|(p, _)| *p == c).unwrap().1;
    assert_eq!(pc, planned);
    assert_ne!(pc, slot_c);
    assert!(config::WORKTREE_PORTS.contains(&pc) && pc.is_multiple_of(10));
    // Recorded: stable, and what `checkout` reads.
    assert_eq!(project.checkout(Some("c"), c.clone()).port, pc);
    assert_eq!(recorded_port(&c), Some(pc));

    // Other projects' worktrees count too.
    let (_d2, mut other, syncer2) = fixture();
    other.name = "other".into();
    let x = create(&other, &syncer2, "x", None).unwrap();
    let slot_d = config::worktree_port("app", "d");
    std::fs::write(
        admin_dir(&x).unwrap().join(PORT_FILE),
        format!("{slot_d}\n"),
    )
    .unwrap();
    let d = create(&project, &syncer, "d", None).unwrap();
    let both = [project.clone(), other.clone()];
    let pd = port_of(&plan_ports(&both).unwrap(), &d);
    assert_ne!(pd, slot_d);
    assert_eq!(
        port_of(&plan_ports(std::slice::from_ref(&project)).unwrap(), &d),
        slot_d
    );
}

#[test]
fn creates_lists_and_removes() {
    let (_d, project, syncer) = fixture();
    let path = create(&project, &syncer, "feat-x", None).unwrap();
    assert_eq!(std::fs::read_to_string(path.join("a.txt")).unwrap(), "a\n");
    // Ignored build caches are carried.
    assert_eq!(
        std::fs::read_to_string(path.join("cache/big")).unwrap(),
        "built\n"
    );
    assert!(!is_dirty(&path).unwrap());

    let infos = list(&project.root).unwrap();
    assert_eq!(infos.len(), 1);
    let info = &infos[0];
    assert_eq!(info.name, "feat-x");
    assert_eq!(info.branch.as_deref(), Some("feat-x"));
    assert!(matches!(
        safety(info, "origin", "main").unwrap(),
        Safety::Safe
    ));

    // A commit of its own needs a merged PR.
    let repo = Repository::open(&path).unwrap();
    std::fs::write(path.join("b.txt"), "b\n").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new("b.txt")).unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = Signature::now("T", "t@example.com").unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let pr_head = repo
        .commit(Some("HEAD"), &sig, &sig, "b", &tree, &[&parent])
        .unwrap();
    let info = &list(&project.root).unwrap()[0];
    assert!(matches!(
        safety(info, "origin", "main").unwrap(),
        Safety::NeedsMergedPr { .. }
    ));
    assert!(covered_by_pr(info, pr_head, "origin", "main").unwrap());

    std::fs::write(path.join("c.txt"), "dirty\n").unwrap();
    assert!(safety(info, "origin", "main").is_err());
    std::fs::remove_file(path.join("c.txt")).unwrap();

    assert_eq!(unpushed(info, "origin", "main").unwrap(), 1);
    remove_files(&project.root, info, true).unwrap();
    assert!(!path.exists());
    assert!(list(&project.root).unwrap().is_empty());
    let repo = Repository::open(&project.root).unwrap();
    assert!(repo.find_branch("feat-x", BranchType::Local).is_err());
    // Can be created again.
    create(&project, &syncer, "feat-x", None).unwrap();
}

#[test]
fn counts_auto_merges_with_changes_of_their_own() {
    let d = TempDir::new().unwrap();
    let repo = Repository::init(d.path()).unwrap();
    let sig = Signature::now("T", "t@example.com").unwrap();
    let commit = |files: &[(&str, &str)], parents: &[Oid], msg: &str| {
        let mut tb = repo.treebuilder(None).unwrap();
        for (name, content) in files {
            tb.insert(name, repo.blob(content.as_bytes()).unwrap(), 0o100644)
                .unwrap();
        }
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let parents: Vec<_> = parents
            .iter()
            .map(|p| repo.find_commit(*p).unwrap())
            .collect();
        let parents: Vec<_> = parents.iter().collect();
        repo.commit(None, &sig, &sig, msg, &tree, &parents).unwrap()
    };
    let a = commit(&[("a", "1")], &[], "init");
    let own = commit(&[("a", "1"), ("b", "own")], &[a], "own");
    let main = commit(&[("a", "2")], &[a], "main");
    let msg = "Merge remote-tracking branch 'origin/main' into x";
    let clean = commit(&[("a", "2"), ("b", "own")], &[own, main], msg);
    assert_eq!(
        ahead(&repo, clean, &[own, main], "origin", "main").unwrap(),
        0
    );
    let evil = commit(
        &[("a", "2"), ("b", "own"), ("c", "extra")],
        &[own, main],
        msg,
    );
    assert_eq!(
        ahead(&repo, evil, &[own, main], "origin", "main").unwrap(),
        1
    );
    // Not what libgit2 would make, but every path is one side's as is (as a
    // `git merge` with other rename detection might do): nothing of its own.
    let other = commit(&[("a", "1"), ("b", "own")], &[own, main], msg);
    assert_eq!(
        ahead(&repo, other, &[own, main], "origin", "main").unwrap(),
        0
    );
    // A hand-resolved path, in neither side nor the clean merge: work.
    let resolved = commit(&[("a", "3"), ("b", "own")], &[own, main], msg);
    assert_eq!(
        ahead(&repo, resolved, &[own, main], "origin", "main").unwrap(),
        1
    );
}

#[test]
fn pushed_commits_are_not_unpushed() {
    let (_d, project, syncer) = fixture();
    let path = create(&project, &syncer, "feat-x", None).unwrap();
    let info = list(&project.root).unwrap().remove(0);
    assert_eq!(unpushed(&info, "origin", "main").unwrap(), 0);
    let repo = Repository::open(&path).unwrap();
    let sig = Signature::now("T", "t@example.com").unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let head = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "own",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap();
    assert_eq!(unpushed(&info, "origin", "main").unwrap(), 1);
    repo.reference("refs/remotes/origin/feat-x", head, true, "push")
        .unwrap();
    assert_eq!(unpushed(&info, "origin", "main").unwrap(), 0);
}

#[test]
fn lists_ignored_files_only_the_worktree_has() {
    let (_d, project, syncer) = fixture();
    let path = create(&project, &syncer, "feat-x", None).unwrap();
    // Carried from the primary (cache/), build caches: quiet.
    std::fs::create_dir(path.join("node_modules")).unwrap();
    std::fs::write(path.join("node_modules/x"), "").unwrap();
    let exclude = project.root.join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(&exclude, "*.local\n.env\n").unwrap();
    std::fs::write(project.root.join("same.local"), "x\n").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(path.join("same.local"), "x\n").unwrap();
    let fifo = CString::new(path.join("pipe.local").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    assert_eq!(
        ignored_files(&project.root, &path).unwrap(),
        Vec::<String>::new()
    );
    // Own files, edited copies, a .env of one's own: reported. A carried cache
    // with more in it isn't.
    std::fs::write(path.join("secrets.local"), "mine\n").unwrap();
    std::fs::write(path.join("same.local"), "edit\n").unwrap();
    std::fs::write(path.join("cache/notes"), "mine\n").unwrap();
    std::fs::write(path.join(".env"), "KEY=1\n").unwrap();
    let mut files = ignored_files(&project.root, &path).unwrap();
    files.sort();
    assert_eq!(files, vec![".env", "same.local", "secrets.local"]);
}

fn set_exclude(project: &Project, lines: &str) {
    let exclude = project.root.join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(&exclude, lines).unwrap();
}

#[test]
fn carried_caches_are_not_reported() {
    let (_d, project, syncer) = fixture();
    set_exclude(&project, "_build/\ndeps/\nassets/\n");
    // Nested and listed in .worktreeinclude, like den_ui/deps.
    std::fs::create_dir_all(project.root.join("sub/deps")).unwrap();
    std::fs::write(project.root.join("sub/.keep"), "").unwrap();
    std::fs::write(
        project.root.join(".worktreeinclude"),
        "cache/\nsub/deps/\nassets/\n",
    )
    .unwrap();
    let path = create(&project, &syncer, "feat-x", None).unwrap();
    // As if git-cow had carried these (only cache/ exists in the primary).
    std::fs::write(populated_marker(&path).unwrap(), "assets\nsub/deps\n").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    // Rebuilt after provisioning: newer than the markers, differs from the primary.
    for dir in ["sub/deps/x", "assets", "cache/more"] {
        std::fs::create_dir_all(path.join(dir)).unwrap();
        std::fs::write(path.join(dir).join("f"), "new\n").unwrap();
    }
    assert_eq!(
        ignored_files(&project.root, &path).unwrap(),
        Vec::<String>::new()
    );
    // Recorded carried paths count without a .worktreeinclude too.
    std::fs::remove_file(project.root.join(".worktreeinclude")).unwrap();
    let files = ignored_files(&project.root, &path).unwrap();
    assert_eq!(files, vec!["cache/"]);
}

#[test]
fn files_from_before_setup_finished_are_not_reported() {
    let (_d, project, syncer) = fixture();
    set_exclude(&project, ".mcp.json\n.claude/\ntmp/\n");
    let path = create(&project, &syncer, "feat-x", None).unwrap();
    // What the setup command writes.
    std::fs::write(path.join(".mcp.json"), "{}\n").unwrap();
    std::fs::create_dir(path.join(".claude")).unwrap();
    std::fs::write(path.join(".claude/settings.json"), "{}\n").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let admin = Repository::open(&path).unwrap().path().to_path_buf();
    std::fs::write(admin.join(SETUP_MARKER), "1\n").unwrap();
    assert_eq!(
        ignored_files(&project.root, &path).unwrap(),
        Vec::<String>::new()
    );
    // Created after: reported, also inside a directory setup made.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(path.join(".claude/notes.md"), "mine\n").unwrap();
    std::fs::create_dir(path.join("tmp")).unwrap();
    std::fs::write(path.join("tmp/dump.sql"), "--\n").unwrap();
    let mut files = ignored_files(&project.root, &path).unwrap();
    files.sort();
    assert_eq!(files, vec![".claude/", "tmp/"]);
    // A setup file edited later: reported.
    std::fs::remove_file(path.join(".claude/notes.md")).unwrap();
    std::fs::remove_dir_all(path.join("tmp")).unwrap();
    std::fs::write(path.join(".mcp.json"), "{\"mine\":1}\n").unwrap();
    let mut files = ignored_files(&project.root, &path).unwrap();
    files.sort();
    assert_eq!(files, vec![".claude/", ".mcp.json"]);
}

#[test]
fn spares_whole_ancestor_chains() {
    // 1 <- 100 (claude) <- 200 (sh) <- 300 (lazy-cow-tree); 1 <- 400 <- 500
    let parent = HashMap::from([(100, 1), (200, 100), (300, 200), (400, 1), (500, 400)]);
    assert_eq!(
        with_ancestors(&parent, &[300, 500]),
        HashSet::from([100, 200, 300, 400, 500])
    );
    // Unknown pids are kept themselves.
    assert_eq!(with_ancestors(&parent, &[42]), HashSet::from([42]));
}

#[test]
fn sweep_does_not_go_through_spared_processes() {
    // claude 100 (inside) <- sh 200 <- lazy-cow-tree 300; claude <- mcp 110 (outside)
    // <- 111; claude <- server 120 (inside) <- watcher 121; other 400 (outside).
    let procs = [
        (100, 1, true),
        (200, 100, false),
        (300, 200, false),
        (110, 100, false),
        (111, 110, false),
        (120, 100, true),
        (121, 120, false),
        (400, 1, false),
    ];
    let mut hit = sweep(&procs, &[300]);
    hit.sort();
    assert_eq!(hit, vec![120, 121]);
}

#[test]
fn removal_prunes_stale_unlocked_but_not_locked_worktrees() {
    let (_d, project, syncer) = fixture();
    let away = create(&project, &syncer, "away", None).unwrap();
    let by_hand = create(&project, &syncer, "by-hand", None).unwrap();
    create(&project, &syncer, "gone", None).unwrap();
    let repo = Repository::open(&project.root).unwrap();
    // Locked on purpose, its volume unmounted: invalid, but not ours to prune.
    repo.find_worktree("away")
        .unwrap()
        .lock(Some("on a usb disk"))
        .unwrap();
    let infos = list(&project.root).unwrap();
    let find = |n: &str| infos.iter().find(|i| i.name == n).unwrap();
    assert!(is_locked(&project.root, find("away")));
    assert!(!is_locked(&project.root, find("gone")));
    std::fs::rename(&away, away.with_file_name("away-unmounted")).unwrap();
    std::fs::remove_dir_all(&by_hand).unwrap();

    remove_files(&project.root, find("gone"), true).unwrap();
    let wt = repo.find_worktree("away").unwrap();
    assert!(matches!(
        wt.is_locked().unwrap(),
        git2::WorktreeLockStatus::Locked(_)
    ));
    assert!(repo.find_worktree("gone").is_err());
    assert!(repo.find_worktree("by-hand").is_err());
}

#[test]
fn cleans_up_trash_of_interrupted_removals() {
    let (_d, project, _) = fixture();
    let trash = project.root.join(".git/lazy-cow-tree-trash/old.123");
    let sibling = project
        .worktrees_dir()
        .join(format!("{SIBLING_TRASH}old.123"));
    let keep = project.worktrees_dir().join("feat-x");
    for d in [&trash, &sibling, &keep] {
        std::fs::create_dir_all(d.join("sub")).unwrap();
    }
    clean_trash(&project.root, &project.worktrees_dir());
    for _ in 0..100 {
        if !trash.exists() && !sibling.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!trash.exists() && !sibling.exists());
    assert!(keep.exists());
}

#[test]
fn removal_can_keep_the_branch() {
    let (_d, project, syncer) = fixture();
    let path = create(&project, &syncer, "keep-me", None).unwrap();
    let info = list(&project.root).unwrap().remove(0);
    assert_eq!(unpushed(&info, "origin", "main").unwrap(), 0);
    std::fs::write(path.join("new.txt"), "x").unwrap();
    std::fs::write(path.join("a.txt"), "changed").unwrap();
    let mut dirty = uncommitted(&path).unwrap();
    dirty.sort();
    assert_eq!(dirty, vec!["a.txt", "new.txt"]);
    let repo = Repository::open(&project.root).unwrap();
    let head = repo.head().unwrap().target().unwrap().to_string();

    let kept = remove_files(&project.root, &info, false).unwrap().unwrap();
    assert_eq!(kept, format!("keep-me-kept-{}", &head[..7]));
    assert!(!info.path.exists());
    assert!(repo.find_branch(&kept, BranchType::Local).is_ok());
    assert!(repo.find_branch("keep-me", BranchType::Local).is_err());
}

#[test]
fn knows_when_a_worktree_was_made() {
    let (_d, project, syncer) = fixture();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    create(&project, &syncer, "feat-x", None).unwrap();
    let info = list(&project.root).unwrap().remove(0);
    let t = created_at(&info).unwrap();
    assert!((before - 1..=before + 60).contains(&t), "{t} vs {before}");
    // Merged 10 minutes later: before. Within the clock-skew grace, or earlier: not.
    assert!(made_before(&info, t + 600).is_ok());
    assert!(made_before(&info, t + 60).is_err());
    assert!(made_before(&info, t - 3600).is_err());
}

#[test]
fn copies_trees_keeping_mtimes_modes_and_links() {
    use std::os::unix::fs::PermissionsExt;
    let d = TempDir::new().unwrap();
    let src = d.path().join("src");
    std::fs::create_dir_all(src.join("lib")).unwrap();
    std::fs::write(src.join("lib/a.beam"), "beam").unwrap();
    std::fs::write(src.join("run"), "#!/bin/sh").unwrap();
    std::fs::set_permissions(src.join("run"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("lib/a.beam", src.join("link")).unwrap();
    let old = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    filetime::set_file_mtime(src.join("lib/a.beam"), old).unwrap();

    let dst = d.path().join("dst");
    cow::copy_tree(&src, &dst).unwrap();
    assert_eq!(
        std::fs::read_to_string(dst.join("lib/a.beam")).unwrap(),
        "beam"
    );
    let meta = std::fs::metadata(dst.join("lib/a.beam")).unwrap();
    assert_eq!(filetime::FileTime::from_last_modification_time(&meta), old);
    let mode = std::fs::metadata(dst.join("run"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(
        std::fs::read_link(dst.join("link")).unwrap(),
        Path::new("lib/a.beam")
    );
}
