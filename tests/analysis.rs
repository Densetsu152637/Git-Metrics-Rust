//! Repository metrics against deterministic local Git fixtures. The end-to-end
//! tests point `https://github.com/` at the fixtures with `url.<base>.insteadOf`
//! so the real clone path runs without network access.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{bare_clone, build_fixture, rewrite_env, unique_name, EnvScope, Fixture};
use git_api::process::RepositoryService;
use git_api::types::{ChangeType, RepositoryData};
use tokio::sync::mpsc;

async fn rewrite_scope(fixture: &Fixture) -> EnvScope {
    let vars = rewrite_env(fixture);
    let borrowed: Vec<(&str, Option<&str>)> =
        vars.iter().map(|(k, v)| (*k, v.as_deref())).collect();
    EnvScope::new(&borrowed).await
}

fn clone_directory(repository: &str) -> PathBuf {
    std::env::temp_dir()
        .join("cloned-repos")
        .join("public")
        .join("github.com")
        .join("owner")
        .join(repository)
}

async fn collect(mut updates: mpsc::Receiver<String>) -> Vec<String> {
    let mut messages = Vec::new();
    while let Some(message) = updates.recv().await {
        messages.push(message);
    }
    messages
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..400 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition not reached in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn formulates_commit_branch_and_contributor_metrics() {
    let fixture = build_fixture("metrics");
    let (_root, clone) = bare_clone(&fixture);
    let service = RepositoryService::new();
    let (progress, updates) = mpsc::channel(1000);
    let data = service
        .formulate_repo_data(&clone, &progress)
        .await
        .unwrap();
    drop(progress);
    let messages = collect(updates).await;

    // Repository name comes from the origin URL's last component.
    assert_eq!(
        data.repo_name,
        fixture
            .repository
            .to_string_lossy()
            .rsplit(['/', '\\'])
            .next()
            .unwrap()
    );

    // Every commit is present exactly once, keyed by hash.
    assert_eq!(data.all_commits.len(), 7);
    for (_, hash) in &fixture.commits {
        assert_eq!(&data.all_commits[hash].commit_hash, hash);
    }

    // Branch names are sorted and each lists its history newest-first by
    // timestamp text, with ties in Git's own order (tie2 was committed last).
    let names: Vec<_> = data
        .branches
        .iter()
        .map(|b| b.branch_name.as_str())
        .collect();
    assert_eq!(names, ["feature", "main"]);
    let order = |labels: &[&str]| -> Vec<String> {
        labels.iter().map(|l| fixture.hash(l).to_owned()).collect()
    };
    assert_eq!(
        data.branches[0].commit_hashes,
        order(&["tie2", "tie1", "rename", "modify", "root"])
    );
    assert_eq!(
        data.branches[1].commit_hashes,
        order(&["merge", "delete", "tie2", "tie1", "rename", "modify", "root"])
    );

    // Root and merge commits have no file records: `diff-tree -p` is run
    // without `--root` or merge expansion.
    for label in ["root", "merge"] {
        assert!(
            data.all_commits[fixture.hash(label)].file_data.is_empty(),
            "{label}"
        );
    }
    assert_eq!(
        data.all_commits[fixture.hash("merge")].contributor_name,
        "Zoë Ünïcode"
    );

    // Modify + add + binary.
    let modify = &data.all_commits[fixture.hash("modify")];
    assert_eq!(modify.timestamp, "2024-01-02T00:00:00+00:00");
    let files: Vec<_> = modify
        .file_data
        .iter()
        .map(|f| (f.filepath.as_str(), f.char))
        .collect();
    assert_eq!(
        files,
        [
            ("a.txt", ChangeType::M),
            ("bin.dat", ChangeType::A),
            ("dir/b.txt", ChangeType::A)
        ]
    );
    let edited = &modify.file_data[0];
    assert_eq!(
        (edited.new_lines, edited.deleted_lines, edited.likeness),
        (1, 1, -1)
    );
    assert_eq!(edited.old_file_path, "a.txt");
    assert_eq!(edited.diff.last().map(String::as_str), Some("+three"));
    assert!(!edited
        .diff
        .iter()
        .any(|line| line.starts_with("diff --git")));
    let binary = &modify.file_data[1];
    assert_eq!((binary.new_lines, binary.deleted_lines), (0, 0));
    assert!(binary
        .diff
        .iter()
        .any(|line| line.starts_with("Binary files")));

    // Rename: `show` detects it, plumbing `diff-tree` does not, so the diff is a
    // full addition of the new path (a retained limitation of the source).
    let rename = &data.all_commits[fixture.hash("rename")];
    assert_eq!(rename.commit_title, "Rename b to c");
    assert_eq!(
        rename.description,
        "Body paragraph one.\n\nIndented second paragraph."
    );
    let renamed = &rename.file_data[0];
    assert_eq!(
        (
            renamed.char,
            renamed.likeness,
            renamed.old_file_path.as_str(),
            renamed.filepath.as_str()
        ),
        (ChangeType::R, 100, "dir/b.txt", "dir/c.txt")
    );
    assert_eq!((renamed.new_lines, renamed.deleted_lines), (3, 0));

    // Deletion.
    let deleted = &data.all_commits[fixture.hash("delete")].file_data[0];
    assert_eq!(
        (deleted.char, deleted.new_lines, deleted.deleted_lines),
        (ChangeType::D, 0, 2)
    );
    assert_eq!(deleted.old_file_path, deleted.filepath);

    // Contributors: unique names, emails in first-seen order, Unicode intact.
    assert_eq!(
        data.contributors
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["Alice", "Bob", "Zoë Ünïcode"]
    );
    assert_eq!(
        data.contributors["Alice"].emails,
        ["alice@example.com", "alice@work.example"]
    );
    assert_eq!(data.contributors["Bob"].emails, ["bob@example.com"]);
    assert_eq!(data.contributors["Zoë Ünïcode"].name, "Zoë Ünïcode");

    // Progress narrates each phase, with a running commit counter.
    assert_eq!(
        messages.first().map(String::as_str),
        Some("Searching for branch names...")
    );
    for expected in [
        "Searching for commit hashes...",
        "Formulating all commit data...",
        "Formulating all commit data (7/7)...",
        "Formulating all contributors...",
        "Linking branches to their commits...",
        "Formulating all branch data...",
        "Fetching repo name...",
    ] {
        assert!(
            messages.iter().any(|m| m == expected),
            "missing {expected:?} in {messages:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_matches_the_documented_wire_shape() {
    let fixture = build_fixture("wire");
    let (_root, clone) = bare_clone(&fixture);
    let service = RepositoryService::new();
    let (progress, _updates) = mpsc::channel(1000);
    let data = service
        .formulate_repo_data(&clone, &progress)
        .await
        .unwrap();
    let value = serde_json::to_value(&data).unwrap();
    let object = value.as_object().unwrap();
    let mut keys: Vec<_> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["allCommits", "branches", "contributors", "repoName"]);
    let commit = &value["allCommits"][fixture.hash("rename")];
    let mut commit_keys: Vec<_> = commit
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    commit_keys.sort_unstable();
    assert_eq!(
        commit_keys,
        [
            "commitHash",
            "commitTitle",
            "contributorName",
            "description",
            "fileData",
            "timestamp"
        ]
    );
    assert_eq!(commit["fileData"][0]["char"], "R");
    let roundtrip: RepositoryData = serde_json::from_value(value).unwrap();
    assert_eq!(roundtrip, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_git_commands_fail_the_analysis() {
    let fixture = build_fixture("failure");
    let (_root, clone) = bare_clone(&fixture);
    let service = RepositoryService::new();
    let (progress, _updates) = mpsc::channel(1000);
    // Not a repository: the first Git command reports `fatal: not a git repository`.
    let empty = std::env::temp_dir().join(unique_name("git-api-not-a-repo"));
    std::fs::create_dir_all(&empty).unwrap();
    // Keep Git from discovering a repository in a parent directory.
    let _env =
        EnvScope::new(&[("GIT_CEILING_DIRECTORIES", empty.parent().unwrap().to_str())]).await;
    let error = service
        .formulate_repo_data(&empty, &progress)
        .await
        .unwrap_err();
    assert!(
        error.starts_with("user error (Failed to parse git branch names:\n"),
        "{error}"
    );
    std::fs::remove_dir_all(&empty).unwrap();

    // A missing directory is reported by the command layer, not a panic.
    let error = service
        .formulate_repo_data(&clone.join("does-not-exist"), &progress)
        .await
        .unwrap_err();
    assert!(error.contains("Invalid filepath: "), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn analyze_clones_processes_and_cleans_up() {
    let fixture = build_fixture("endtoend");
    let _env = rewrite_scope(&fixture).await;
    let service = RepositoryService::new();
    let (progress, updates) = mpsc::channel(1000);

    let data = service
        .analyze(
            "https://github.com/owner/endtoend.git".to_owned(),
            None,
            progress,
        )
        .await
        .unwrap();
    let messages = collect(updates).await;

    assert_eq!(data.all_commits.len(), 7);
    assert_eq!(data.branches.len(), 2);
    let position = |text: &str| {
        messages
            .iter()
            .position(|m| m == text)
            .unwrap_or_else(|| panic!("missing {text:?} in {messages:?}"))
    };
    let order = [
        "Validating repo exists...",
        "Found the repo!",
        "Cloning repo...",
        "Getting repository data...",
        "Searching for branch names...",
        "Data processed!",
        "Cleaning Up Directory...",
    ];
    let positions: Vec<_> = order.iter().map(|text| position(text)).collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{positions:?} {messages:?}"
    );

    // The credential-scoped clone directory is removed when the last user is done.
    assert!(!clone_directory("endtoend").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn analyze_reports_validation_and_missing_repository_errors() {
    let fixture = build_fixture("errors");
    let _env = rewrite_scope(&fixture).await;
    let service = RepositoryService::new();

    // Token validation wins over URL validation.
    let (progress, updates) = mpsc::channel(1000);
    let error = service
        .analyze(
            "http://example.com/x".to_owned(),
            Some(String::new()),
            progress,
        )
        .await
        .unwrap_err();
    assert_eq!(error, "auth_token must not be empty.");
    assert_eq!(collect(updates).await, ["auth_token must not be empty."]);

    let (progress, updates) = mpsc::channel(1000);
    let error = service
        .analyze("http://github.com/owner/errors".to_owned(), None, progress)
        .await
        .unwrap_err();
    assert_eq!(error, "Repository URL must use HTTPS.");
    assert_eq!(collect(updates).await, ["Repository URL must use HTTPS."]);

    // A repository that does not exist fails the probe and leaves nothing behind.
    let (progress, updates) = mpsc::channel(1000);
    let error = service
        .analyze(
            "https://github.com/owner/missing".to_owned(),
            None,
            progress,
        )
        .await
        .unwrap_err();
    assert!(
        error.starts_with("user error (Repo does not exist:\n"),
        "{error}"
    );
    assert!(
        error.contains("Could not find Repo (https://github.com/owner/missing): "),
        "{error}"
    );
    let messages = collect(updates).await;
    assert_eq!(
        messages.first().map(String::as_str),
        Some("Validating repo exists...")
    );
    assert!(messages
        .last()
        .unwrap()
        .starts_with("Encountered error:\nuser error (Repo does not exist"));
    assert!(!clone_directory("missing").exists());

    // The failure was not cached: a later valid request succeeds.
    let (progress, _updates) = mpsc::channel(1000);
    assert!(service
        .analyze("https://github.com/owner/errors".to_owned(), None, progress)
        .await
        .is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_share_one_analysis_and_only_the_leader_streams() {
    let fixture = build_fixture("shared");
    let _env = rewrite_scope(&fixture).await;
    let service = Arc::new(RepositoryService::new());
    let url = "https://github.com/owner/shared".to_owned();

    let (leader_progress, mut leader_updates) = mpsc::channel(1000);
    let leader = {
        let (service, url) = (service.clone(), url.clone());
        tokio::spawn(async move { service.analyze(url, None, leader_progress).await })
    };
    // The leader is in flight once it reports progress.
    assert_eq!(
        leader_updates.recv().await.as_deref(),
        Some("Validating repo exists...")
    );

    let mut followers = Vec::new();
    for _ in 0..4 {
        let (progress, updates) = mpsc::channel(1000);
        let (service, url) = (service.clone(), url.clone());
        followers.push((
            tokio::spawn(async move { service.analyze(url, None, progress).await }),
            updates,
        ));
    }

    let leader_data = leader.await.unwrap().unwrap();
    for (follower, updates) in followers {
        let shared = follower.await.unwrap().unwrap();
        assert!(
            Arc::ptr_eq(&leader_data, &shared),
            "follower did not share the leader's result"
        );
        assert!(
            collect(updates).await.is_empty(),
            "followers must not receive progress"
        );
    }
    let mut remaining = Vec::new();
    while let Ok(message) = leader_updates.try_recv() {
        remaining.push(message);
    }
    assert!(remaining.iter().any(|m| m == "Data processed!"));

    // Completed batches are not cached: a new request does a fresh analysis.
    let (progress, _updates) = mpsc::channel(1000);
    let again = service.analyze(url, None, progress).await.unwrap();
    assert!(!Arc::ptr_eq(&leader_data, &again));
    assert_eq!(*leader_data, *again);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credential_scopes_are_analysed_independently() {
    let fixture = build_fixture("scoped");
    let _env = rewrite_scope(&fixture).await;
    let service = Arc::new(RepositoryService::new());
    let url = "https://github.com/owner/scoped".to_owned();

    let run = |token: Option<&'static str>| {
        let (service, url) = (service.clone(), url.clone());
        tokio::spawn(async move {
            let (progress, _updates) = mpsc::channel(1000);
            service
                .analyze(url, token.map(str::to_owned), progress)
                .await
        })
    };
    let (public, first, second) = (run(None), run(Some("token-one")), run(Some("token-two")));
    let (public, first, second) = (
        public.await.unwrap().unwrap(),
        first.await.unwrap().unwrap(),
        second.await.unwrap().unwrap(),
    );
    assert!(!Arc::ptr_eq(&public, &first));
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(*public, *first);
    assert_eq!(*first, *second);

    // No scoped directory (public or authenticated) survives.
    let root = std::env::temp_dir().join("cloned-repos");
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().join("github.com/owner/scoped").exists())
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_analysis_cleans_up_and_can_be_retried() {
    let fixture = build_fixture("cancelled");
    let _env = rewrite_scope(&fixture).await;
    let service = Arc::new(RepositoryService::new());
    let url = "https://github.com/owner/cancelled".to_owned();

    let (progress, mut updates) = mpsc::channel(1000);
    let task = {
        let (service, url) = (service.clone(), url.clone());
        tokio::spawn(async move { service.analyze(url, None, progress).await })
    };
    // Cancel once the clone exists and processing has started.
    while let Some(message) = updates.recv().await {
        if message == "Getting repository data..." {
            break;
        }
    }
    task.abort();
    let _ = task.await;

    wait_until(|| !clone_directory("cancelled").exists()).await;

    let (progress, _updates) = mpsc::channel(1000);
    let data = service.analyze(url, None, progress).await.unwrap();
    assert_eq!(data.all_commits.len(), 7);
}
