//! Integration tests against real temporary repositories.

use std::path::Path;

use cmdterm::vcs::{VcsError, VcsKind, clone, clone_name, discover, discover_with};
use cmdterm_env::Environment;

const ENV: Environment = Environment::Local;

fn run(cwd: &Path, program: &str, args: &[&str]) -> Vec<u8> {
    ENV.command(program)
        .args(args)
        .cwd(cwd)
        .var("HGPLAIN", "1")
        .var("HGUSER", "test <t@t>")
        .var("JJ_USER", "test")
        .var("JJ_EMAIL", "t@t")
        .stdout()
        .unwrap_or_else(|e| panic!("{program} {args:?}: {e}"))
}

fn git(cwd: &Path, args: &[&str]) {
    let mut full = vec![
        "-c",
        "user.name=test",
        "-c",
        "user.email=t@t",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
    ];
    full.extend_from_slice(args);
    run(cwd, "git", &full);
}

fn write(path: &Path, text: &str) {
    ENV.command("sh")
        .args(["-c", "printf '%s' \"$2\" > \"$1\"", "sh"])
        .arg(path)
        .arg(text)
        .stdout()
        .unwrap();
}

fn have(program: &str) -> bool {
    ENV.command("sh")
        .args(["-c", "command -v \"$1\" >/dev/null", "sh", program])
        .stdout()
        .is_ok()
}

/// Canonical path, since git reports symlink-resolved paths.
fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

#[test]
fn git_repository_end_to_end() {
    let (_dir, base) = temp();
    let root = base.join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root.join("a.txt"), "one\n");
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    git(
        &root,
        &["remote", "add", "origin", "https://gitlab.com/g/r.git"],
    );

    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let repo = discover(&ENV, &sub).unwrap();
    assert_eq!(repo.kind, VcsKind::Git);
    assert_eq!(repo.root, root);

    let wt_path = base.join("wt");
    let wt = repo.add_worktree(&wt_path, "feature", true).unwrap();
    assert_eq!(wt.branch.as_deref(), Some("feature"));
    assert_eq!(wt.head.as_ref().map(String::len), Some(40));

    // A linked worktree maps back to the main one, which now lists it.
    let again = discover(&ENV, &wt_path).unwrap();
    assert_eq!((&again.kind, &again.root), (&repo.kind, &repo.root));
    let dirs = again.git.as_ref().unwrap();
    assert_eq!(dirs.common, repo.git.as_ref().unwrap().common);
    assert!(dirs.linked.iter().any(|(path, _)| *path == wt_path));
    // Git calls only go to worktrees the repository lists, so work on the
    // one opened after the worktree was added, as the app's worker does.
    let repo = again;

    let list = repo.worktrees().unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0].is_main && list[0].path == root);
    assert_eq!(list[0].branch.as_deref(), Some("main"));
    assert_eq!(list[1].path, wt_path);
    assert_eq!(list[1].head, wt.head);

    let mut branches = repo.branches().unwrap();
    branches.sort();
    assert_eq!(branches, ["feature", "main"]);

    let remotes = repo.remotes().unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].name, "origin");
    assert_eq!(remotes[0].url, "https://gitlab.com/g/r.git");

    // Status: one staged, one modified, one untracked.
    write(&wt_path.join("a.txt"), "two\n");
    write(&wt_path.join("b.txt"), "new\n");
    git(&wt_path, &["add", "b.txt"]);
    write(&wt_path.join("c.txt"), "untracked\n");
    let status = repo.status(&wt_path).unwrap();
    assert_eq!(status.branch.as_deref(), Some("feature"));
    assert_eq!(
        (status.staged, status.modified, status.untracked),
        (1, 1, 1)
    );
    assert_eq!(status.conflicted, 0);
    assert_eq!(repo.status(&root).unwrap().modified, 0);

    // Dirty worktree needs force.
    assert!(matches!(
        repo.remove_worktree(&wt_path, false),
        Err(VcsError::Command(_))
    ));
    repo.remove_worktree(&wt_path, true).unwrap();
    assert_eq!(repo.worktrees().unwrap().len(), 1);

    // Existing branch, relative path.
    let wt2 = repo
        .add_worktree(Path::new("../wt2"), "feature", false)
        .unwrap();
    assert_eq!(wt2.path, base.join("wt2"));
    let repo = discover(&ENV, &root).unwrap();
    assert!(repo.remove_worktree(&root, true).is_err());

    // From another worktree's commit: detached, then on a new branch.
    write(&wt2.path.join("a.txt"), "three\n");
    git(&wt2.path, &["commit", "-q", "-am", "on feature"]);
    let feature_head = repo.worktrees().unwrap()[1].head.clone();
    let detached = repo
        .add_worktree_from(&wt2.path, &base.join("wt3"), None)
        .unwrap();
    assert_eq!(detached.branch, None);
    assert_eq!(detached.head, feature_head);
    let repo = discover(&ENV, &root).unwrap();
    let list = repo.worktrees().unwrap();
    let listed = list.iter().find(|w| w.path == detached.path).unwrap();
    assert_eq!(
        (listed.branch.as_deref(), &listed.head),
        (None, &feature_head)
    );
    let branched = repo
        .add_worktree_from(&detached.path, &base.join("wt4"), Some("topic"))
        .unwrap();
    assert_eq!(branched.branch.as_deref(), Some("topic"));
    assert_eq!(branched.head, feature_head);
    assert!(
        repo.add_worktree_from(&root, &base.join("wt5"), Some("-x"))
            .is_err()
    );

    // At a named revision: a new branch there, or detached.
    let at = repo
        .add_worktree_at(&base.join("wt6"), "feature", Some("from-feature"))
        .unwrap();
    assert_eq!(
        (at.branch.as_deref(), &at.head),
        (Some("from-feature"), &feature_head)
    );
    let at = repo
        .add_worktree_at(&base.join("wt7"), "feature~1", None)
        .unwrap();
    assert_eq!(at.branch, None);
    assert_ne!(at.head, feature_head);
    assert!(
        repo.add_worktree_at(&base.join("wt8"), "--orphan", None)
            .is_err()
    );
    assert!(
        repo.add_worktree_at(&base.join("wt8"), "nope", None)
            .is_err()
    );
}

#[test]
fn git_conflicts_and_ahead() {
    let (_dir, base) = temp();
    let upstream = base.join("up");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main"]);
    write(&upstream.join("f"), "base\n");
    git(&upstream, &["add", "f"]);
    git(&upstream, &["commit", "-q", "-m", "base"]);
    git(&base, &["clone", "-q", "up", "clone"]);
    let clone = base.join("clone");
    write(&clone.join("f"), "ours\n");
    git(&clone, &["commit", "-q", "-am", "ours"]);
    git(&clone, &["checkout", "-q", "-b", "other", "HEAD~1"]);
    write(&clone.join("f"), "theirs\n");
    git(&clone, &["commit", "-q", "-am", "theirs"]);
    git(&clone, &["checkout", "-q", "main"]);
    let _ = ENV
        .command("git")
        .args(["merge", "-q", "other"])
        .cwd(&clone)
        .output();

    let repo = discover(&ENV, &clone).unwrap();
    let status = repo.status(&clone).unwrap();
    assert_eq!(status.conflicted, 1);
    assert_eq!((status.ahead, status.behind), (1, 0));
    assert!(repo.remotes().unwrap()[0].url.ends_with("up"));
}

#[test]
fn git_bare_and_no_remotes() {
    let (_dir, base) = temp();
    let bare = base.join("b.git");
    std::fs::create_dir(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare"]);
    let repo = discover(&ENV, &bare).unwrap();
    assert_eq!(repo.root, bare);
    assert!(repo.remotes().unwrap().is_empty());
    let list = repo.worktrees().unwrap();
    assert!(list[0].bare && list[0].is_main);
}

#[test]
fn not_a_repository() {
    let (_dir, base) = temp();
    assert!(matches!(
        discover(&ENV, &base),
        Err(VcsError::NotARepository(_))
    ));
}

#[test]
fn mercurial_shares() {
    if !have("hg") {
        eprintln!("hg not installed; skipping");
        return;
    }
    let (_dir, base) = temp();
    let root = base.join("hgrepo");
    run(&base, "hg", &["init", "hgrepo"]);
    write(&root.join("a"), "one\n");
    run(&root, "hg", &["add", "a"]);
    run(&root, "hg", &["commit", "-q", "-m", "init"]);
    run(&root, "hg", &["bookmark", "main"]);
    write(
        &root.join(".hg/hgrc"),
        "[paths]\ndefault = https://codeberg.org/o/r\n",
    );

    let repo = discover(&ENV, &root).unwrap();
    assert_eq!(repo.kind, VcsKind::Mercurial);
    assert_eq!(repo.root, root);
    let remotes = repo.remotes().unwrap();
    assert_eq!(remotes[0].name, "default");
    assert_eq!(remotes[0].url, "https://codeberg.org/o/r");

    let share = base.join("share");
    let wt = repo.add_worktree(&share, "topic", true).unwrap();
    assert_eq!(wt.head.as_ref().map(String::len), Some(40));
    assert_eq!(discover(&ENV, &share).unwrap(), repo);

    let list = repo.worktrees().unwrap();
    assert_eq!(list.len(), 2, "{list:?}");
    assert!(list[0].is_main);
    assert_eq!(list[0].branch.as_deref(), Some("main"));
    assert_eq!(list[1].path, share);
    assert_eq!(list[1].branch.as_deref(), Some("topic"));

    let branches = repo.branches().unwrap();
    assert!(branches.contains(&"default".to_owned()), "{branches:?}");
    assert!(branches.contains(&"topic".to_owned()), "{branches:?}");
    assert!(branches.contains(&"main".to_owned()), "{branches:?}");

    write(&share.join("a"), "two\n");
    write(&share.join("u"), "x\n");
    let status = repo.status(&share).unwrap();
    assert_eq!(status.branch.as_deref(), Some("topic"));
    assert_eq!(
        (status.modified, status.untracked, status.staged),
        (1, 1, 0)
    );

    assert!(repo.remove_worktree(&share, false).is_err());
    repo.remove_worktree(&share, true).unwrap();
    assert!(!share.exists());
    assert_eq!(repo.worktrees().unwrap().len(), 1);

    // Existing bookmark; then the directory vanishes and is reported prunable.
    let share2 = base.join("share2");
    let wt2 = repo.add_worktree(&share2, "main", false).unwrap();
    assert_eq!(wt2.branch.as_deref(), Some("main"));
    std::fs::remove_dir_all(&share2).unwrap();
    let list = repo.worktrees().unwrap();
    assert!(list[1].prunable);
    repo.remove_worktree(&share2, false).unwrap();
    assert_eq!(repo.worktrees().unwrap().len(), 1);

    // From a share's revision: no active bookmark, then a new one.
    let share3 = base.join("share3");
    repo.add_worktree(&share3, "topic", false).unwrap();
    write(&share3.join("a"), "three\n");
    run(&share3, "hg", &["commit", "-q", "-m", "on topic"]);
    let topic_head = repo.worktrees().unwrap()[1].head.clone();
    let detached = repo
        .add_worktree_from(&share3, &base.join("share4"), None)
        .unwrap();
    assert_eq!(detached.head, topic_head);
    let list = repo.worktrees().unwrap();
    let listed = list.iter().find(|w| w.path == detached.path).unwrap();
    // No bookmark active: hg reports the named branch.
    assert_eq!(listed.branch.as_deref(), Some("default"));
    let branched = repo
        .add_worktree_from(&share3, &base.join("share5"), Some("fresh"))
        .unwrap();
    assert_eq!(branched.head, topic_head);
    let list = repo.worktrees().unwrap();
    let listed = list.iter().find(|w| w.path == branched.path).unwrap();
    assert_eq!(listed.branch.as_deref(), Some("fresh"));
    // At a named revision or revset: a new bookmark there, or none.
    let at = repo
        .add_worktree_at(&base.join("share6"), "fresh", Some("from-fresh"))
        .unwrap();
    assert_eq!(
        (at.branch.as_deref(), &at.head),
        (Some("from-fresh"), &topic_head)
    );
    let at_detached = repo
        .add_worktree_at(&base.join("share7"), "max(bookmark(fresh))", None)
        .unwrap();
    assert_eq!(at_detached.head, topic_head);
    assert!(
        repo.add_worktree_at(&base.join("share8"), "none()", None)
            .is_err()
    );
    for path in [&at.path, &at_detached.path] {
        repo.remove_worktree(path, true).unwrap();
    }
    for path in [&share3, &detached.path, &branched.path] {
        repo.remove_worktree(path, true).unwrap();
    }
    assert_eq!(repo.worktrees().unwrap().len(), 1);
    assert!(repo.remove_worktree(&base.join("unrelated"), true).is_err());
}

/// The listing as sorted strings.
fn file_list(out: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = out
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn files_list_tracked_and_unignored_files() {
    let (_dir, root) = temp();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir(root.join("src")).unwrap();
    write(&root.join("src/a.rs"), "a\n");
    write(&root.join(".gitignore"), "target/\n");
    git(&root, &["add", "."]);
    write(&root.join("new.txt"), "n\n");
    std::fs::create_dir(root.join("target")).unwrap();
    write(&root.join("target/out"), "o\n");
    let repo = discover(&ENV, &root).unwrap();
    assert_eq!(
        file_list(&repo.files(&root).unwrap()),
        [".gitignore", "new.txt", "src/a.rs"]
    );

    if !have("hg") {
        eprintln!("hg not installed; skipping the Mercurial half");
        return;
    }
    let (_dir, root) = temp();
    run(&root, "hg", &["init"]);
    write(&root.join("tracked"), "t\n");
    run(&root, "hg", &["add", "tracked"]);
    write(&root.join(".hgignore"), "^skip$\n");
    write(&root.join("skip"), "s\n");
    write(&root.join("new"), "n\n");
    let repo = discover(&ENV, &root).unwrap();
    assert_eq!(
        file_list(&repo.files(&root).unwrap()),
        [".hgignore", "new", "tracked"]
    );
}

#[test]
fn clone_names_follow_git() {
    assert_eq!(clone_name("https://example.com/org/app.git"), Some("app"));
    assert_eq!(clone_name("https://example.com/org/app/"), Some("app"));
    assert_eq!(clone_name("git@example.com:app.git"), Some("app"));
    assert_eq!(clone_name("ssh://hg@example.com/repo"), Some("repo"));
    assert_eq!(clone_name("https://example.com/.git"), None);
    assert_eq!(clone_name("https://example.com/-x"), None);
}

#[test]
fn clone_creates_the_parent_and_reopens_an_existing_checkout() {
    let (_dir, base) = temp();
    let upstream = base.join("up");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main"]);
    write(&upstream.join("f"), "base\n");
    git(&upstream, &["add", "f"]);
    git(&upstream, &["commit", "-q", "-m", "base"]);
    let url = format!("file://{}", upstream.display());

    let parent = base.join("src/nested");
    let repo = clone(&ENV, &url, &parent).unwrap();
    assert_eq!(repo.kind, VcsKind::Git);
    assert_eq!(repo.root, parent.canonicalize().unwrap().join("up"));
    assert!(repo.root.join("f").is_file());
    // A second clone of the same URL opens the checkout that is there.
    assert_eq!(clone(&ENV, &url, &parent).unwrap().root, repo.root);

    let missing = format!("file://{}", base.join("missing").display());
    assert!(clone(&ENV, &missing, &parent).is_err());
}

/// Two commits on `main`, then a feature branch with two more.
#[test]
fn git_log_show_and_review_against_the_base() {
    let (_dir, base) = temp();
    let root = base.join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    let repo = discover(&ENV, &root).unwrap();
    assert!(repo.log(&root).unwrap().is_empty(), "no commits yet");
    write(&root.join("a.txt"), "one\ntwo\n");
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    write(&root.join("a.txt"), "one\ntwo\nthree\n");
    git(&root, &["commit", "-q", "-am", "three"]);
    git(&root, &["checkout", "-q", "-b", "feature"]);
    write(&root.join("a.txt"), "one\n2\nthree\n");
    git(&root, &["commit", "-q", "-am", "two is a digit"]);
    write(&root.join("new file.txt"), "fresh\n");
    git(&root, &["add", "new file.txt"]);
    git(&root, &["commit", "-q", "-m", "add a file\n\nWith a body."]);

    let log = repo.log(&root).unwrap();
    let subjects: Vec<&str> = log.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["add a file", "two is a digit", "three", "init"]);
    assert!(log[0].refs.contains("feature"), "{:?}", log[0].refs);
    assert_eq!(log[0].author, "test");
    assert!(log[0].id.starts_with(&log[0].short));

    let shown = String::from_utf8(repo.show(&root, &log[0].id).unwrap()).unwrap();
    assert!(shown.contains("With a body."), "{shown}");
    assert!(
        shown.contains("diff --git a/new file.txt b/new file.txt"),
        "{shown}"
    );
    assert!(shown.contains("+fresh"), "{shown}");
    assert!(repo.show(&root, "--all").is_err());

    assert_eq!(repo.default_base(&root).unwrap().as_deref(), Some("main"));
    let review = repo.review(&root, "main", false).unwrap();
    assert_eq!(review.merge_base, log[2].id);
    assert_eq!(review.commits.len(), 2);
    let patch = String::from_utf8(review.patch).unwrap();
    assert!(patch.contains("-two\n+2\n"), "{patch}");
    assert!(patch.contains("+fresh"), "{patch}");
    assert!(!patch.contains("+three"), "base changes stay out: {patch}");

    // Staged, unstaged and untracked changes count only when asked for.
    write(&root.join("a.txt"), "one\n2\nthree\nfour\n");
    write(&root.join("staged.txt"), "added\n");
    git(&root, &["add", "staged.txt"]);
    write(&root.join("untracked.txt"), "loose\n");
    write(&root.join(".gitignore"), "ignored.txt\n");
    write(&root.join("ignored.txt"), "hidden\n");
    let committed = repo.review(&root, "main", false).unwrap();
    let committed = String::from_utf8_lossy(&committed.patch);
    for line in ["+four", "+added", "+loose"] {
        assert!(!committed.contains(line), "{committed}");
    }
    let all = repo.review(&root, "main", true).unwrap();
    let all = String::from_utf8_lossy(&all.patch);
    for line in ["+four", "+added", "+loose", "new file mode 100644"] {
        assert!(all.contains(line), "{all}");
    }
    assert!(!all.contains("+hidden"), "{all}");
    assert!(repo.review(&root, "no-such-branch", false).is_err());
}

#[test]
fn git_default_base_prefers_the_remote_head() {
    let (_dir, base) = temp();
    let upstream = base.join("up");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "trunk"]);
    write(&upstream.join("f"), "x\n");
    git(&upstream, &["add", "f"]);
    git(&upstream, &["commit", "-q", "-m", "base"]);
    git(&base, &["clone", "-q", "up", "clone"]);
    let clone = base.join("clone");
    git(&clone, &["branch", "main"]);
    let repo = discover(&ENV, &clone).unwrap();
    assert_eq!(
        repo.default_base(&clone).unwrap().as_deref(),
        Some("origin/trunk")
    );
}

#[test]
fn mercurial_log_show_and_review() {
    if !have("hg") {
        eprintln!("hg not installed; skipping");
        return;
    }
    let (_dir, base) = temp();
    let root = base.join("hgrepo");
    run(&base, "hg", &["init", "hgrepo"]);
    write(&root.join("a"), "one\n");
    run(&root, "hg", &["add", "a"]);
    run(&root, "hg", &["commit", "-q", "-m", "init"]);
    run(&root, "hg", &["branch", "-q", "feature"]);
    write(&root.join("a"), "one\ntwo\n");
    run(&root, "hg", &["commit", "-q", "-m", "two\n\nbody"]);
    run(&root, "hg", &["bookmark", "mine"]);

    let repo = discover(&ENV, &root).unwrap();
    let log = repo.log(&root).unwrap();
    let subjects: Vec<&str> = log.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["two", "init"]);
    assert_eq!(log[0].refs, "mine");
    let shown = String::from_utf8(repo.show(&root, &log[0].id).unwrap()).unwrap();
    assert!(shown.contains("body") && shown.contains("+two"), "{shown}");

    assert_eq!(
        repo.default_base(&root).unwrap().as_deref(),
        Some("default")
    );
    let review = repo.review(&root, "default", false).unwrap();
    assert_eq!(review.merge_base, log[1].id);
    assert_eq!(review.commits.len(), 1);
    let patch = String::from_utf8(review.patch).unwrap();
    assert!(
        patch.contains("diff --git a/a b/a") && patch.contains("+two"),
        "{patch}"
    );

    write(&root.join("loose"), "new\n");
    let committed = repo.review(&root, "default", false).unwrap();
    assert!(!String::from_utf8_lossy(&committed.patch).contains("+new"));
    let all = repo.review(&root, "default", true).unwrap();
    let all = String::from_utf8_lossy(&all.patch);
    assert!(
        all.contains("diff --git a/loose b/loose") && all.contains("+new"),
        "{all}"
    );
}

#[test]
fn jujutsu_workspaces() {
    if !have("jj") {
        eprintln!("jj not installed; skipping");
        return;
    }
    let (_dir, base) = temp();
    let root = base.join("jjrepo");
    run(&base, "jj", &["git", "init", "--colocate", "jjrepo"]);
    write(&root.join("a"), "one\n");
    run(&root, "jj", &["commit", "-m", "init"]);
    run(&root, "jj", &["bookmark", "create", "main", "-r", "@-"]);
    run(
        &root,
        "jj",
        &["git", "remote", "add", "origin", "https://codeberg.org/o/r"],
    );

    // Colocated: git unless jj is turned on.
    assert_eq!(discover(&ENV, &root).unwrap().kind, VcsKind::Git);
    let repo = discover_with(&ENV, &root, true).unwrap();
    assert_eq!(repo.kind, VcsKind::Jujutsu);
    assert_eq!(repo.root, root);
    let remotes = repo.remotes().unwrap();
    assert_eq!(remotes[0].name, "origin");
    assert_eq!(remotes[0].url, "https://codeberg.org/o/r");

    let ws = base.join("ws");
    let wt = repo.add_worktree(&ws, "topic", true).unwrap();
    assert_eq!(wt.branch.as_deref(), Some("topic"));
    assert_eq!(wt.head.as_ref().map(String::len), Some(32));
    assert_eq!(discover_with(&ENV, &ws, true).unwrap(), repo);

    let list = repo.worktrees().unwrap();
    assert_eq!(list.len(), 2, "{list:?}");
    assert!(list[0].is_main);
    assert_eq!(list[0].branch.as_deref(), Some("main"));
    assert_eq!(list[1].path, ws);
    assert_eq!(list[1].branch.as_deref(), Some("topic"));

    let branches = repo.branches().unwrap();
    assert!(branches.contains(&"main".to_owned()), "{branches:?}");
    assert!(branches.contains(&"topic".to_owned()), "{branches:?}");

    write(&ws.join("a"), "two\n");
    write(&ws.join("u"), "x\n");
    let status = repo.status(&ws).unwrap();
    assert_eq!(status.branch.as_deref(), Some("topic"));
    assert_eq!(
        (status.modified, status.untracked, status.staged),
        (2, 0, 0)
    );
    let files = repo.files(&ws).unwrap();
    assert!(files.split(|&b| b == 0).any(|f| f == b"u"), "{files:?}");

    let log = repo.log(&root).unwrap();
    assert_eq!(log[0].subject, "init");
    assert_eq!(log[0].refs, "main");
    let shown = repo.show(&root, &log[0].id).unwrap();
    let shown = String::from_utf8_lossy(&shown);
    // jj puts the hunk's heading after `@@`, git leaves it empty; the
    // patch itself is the same.
    assert!(shown.contains("+++ b/a\n@@ -0,0 +1,1 @@"), "{shown}");
    assert!(shown.contains("\n+one"), "{shown}");

    let review = repo.review(&ws, "main", true).unwrap();
    assert_eq!(review.merge_base, log[0].id);
    assert_eq!(review.commits.len(), 1);
    let patch = String::from_utf8_lossy(&review.patch);
    assert!(patch.contains("diff --git a/u b/u"), "{patch}");
    let committed = repo.review(&ws, "main", false).unwrap();
    assert!(committed.commits.is_empty() && committed.patch.is_empty());

    repo.remove_worktree(&ws, false).unwrap();
    assert!(!ws.exists());
    assert_eq!(repo.worktrees().unwrap().len(), 1);

    // Existing bookmark; then the directory vanishes and is reported prunable.
    let ws2 = base.join("ws2");
    let wt2 = repo.add_worktree(&ws2, "main", false).unwrap();
    assert_eq!(wt2.branch.as_deref(), Some("main"));
    std::fs::remove_dir_all(&ws2).unwrap();
    let list = repo.worktrees().unwrap();
    assert!(list[1].prunable, "{list:?}");
    repo.remove_worktree(&ws2, false).unwrap();
    assert_eq!(repo.worktrees().unwrap().len(), 1);

    // At a revset, without a bookmark: the parent's names the worktree.
    let at = repo
        .add_worktree_at(&base.join("ws3"), "bookmarks(main)", None)
        .unwrap();
    assert_eq!(at.branch, None);
    let list = repo.worktrees().unwrap();
    let listed = list.iter().find(|w| w.path == at.path).unwrap();
    assert_eq!(listed.branch.as_deref(), Some("main"));
    assert!(repo.remove_worktree(&base.join("elsewhere"), true).is_err());
}
