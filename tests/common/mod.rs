//! Shared test support: serialised environment changes and local Git fixtures.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Mutex, MutexGuard};

static ENVIRONMENT: Mutex<()> = Mutex::const_new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Holds the process-wide environment lock and restores changed variables on drop.
pub struct EnvScope {
    saved: Vec<(String, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvScope {
    pub async fn new(vars: &[(&str, Option<&str>)]) -> Self {
        let lock = ENVIRONMENT.lock().await;
        let mut saved = Vec::new();
        for (name, value) in vars {
            saved.push(((*name).to_owned(), std::env::var_os(name)));
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        Self { saved, _lock: lock }
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(&name, value),
                None => std::env::remove_var(&name),
            }
        }
    }
}

pub fn unique_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

pub fn git_ok(directory: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .envs(env.iter().copied())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git must be installed to run the tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A working repository with a deterministic history.
pub struct Fixture {
    pub root: PathBuf,
    pub repository: PathBuf,
    /// `git rev-parse` of the commits, by label.
    pub commits: Vec<(&'static str, String)>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    pub fn hash(&self, label: &str) -> &str {
        &self
            .commits
            .iter()
            .find(|(name, _)| *name == label)
            .unwrap_or_else(|| panic!("no commit labelled {label}"))
            .1
    }

    /// `fixtures-root/` with a trailing slash, for `url.<base>.insteadOf`.
    pub fn base_url(&self) -> String {
        format!("{}/", self.root.to_string_lossy().replace('\\', "/"))
    }
}

fn commit(
    repository: &Path,
    message: &str,
    author: (&str, &str),
    date: &str,
    extra: &[&str],
) -> String {
    let env = [
        ("GIT_AUTHOR_NAME", author.0),
        ("GIT_AUTHOR_EMAIL", author.1),
        ("GIT_AUTHOR_DATE", date),
        ("GIT_COMMITTER_NAME", author.0),
        ("GIT_COMMITTER_EMAIL", author.1),
        ("GIT_COMMITTER_DATE", date),
    ];
    let mut args = vec!["commit", "--quiet", "--allow-empty", "-m", message];
    args.extend_from_slice(extra);
    git_ok(repository, &args, &env);
    git_ok(repository, &["rev-parse", "HEAD"], &[])
        .trim()
        .to_owned()
}

fn write(repository: &Path, name: &str, content: &[u8]) {
    let path = repository.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn merge(repository: &Path, branch: &str, author: (&str, &str), date: &str) -> String {
    let env = [
        ("GIT_AUTHOR_NAME", author.0),
        ("GIT_AUTHOR_EMAIL", author.1),
        ("GIT_AUTHOR_DATE", date),
        ("GIT_COMMITTER_NAME", author.0),
        ("GIT_COMMITTER_EMAIL", author.1),
        ("GIT_COMMITTER_DATE", date),
    ];
    git_ok(
        repository,
        &["merge", "--no-ff", "--quiet", "-m", "Merge feature", branch],
        &env,
    );
    git_ok(repository, &["rev-parse", "HEAD"], &[])
        .trim()
        .to_owned()
}

const ALICE: (&str, &str) = ("Alice", "alice@example.com");
const ALICE_WORK: (&str, &str) = ("Alice", "alice@work.example");
const BOB: (&str, &str) = ("Bob", "bob@example.com");
const ZOE: (&str, &str) = ("Zoë Ünïcode", "zoe@example.com");

/// Creates `<root>/owner/<name>` with this history (oldest first):
/// `root` (add a.txt), `modify` (edit a.txt, add dir/b.txt, binary file),
/// then on `feature`: `rename` (dir/b.txt -> dir/c.txt), `tie1` and `tie2`
/// sharing one timestamp; on `main`: `delete` (a.txt), and a `merge` commit.
pub fn build_fixture(name: &str) -> Fixture {
    let root = std::env::temp_dir().join(unique_name("git-api-fixture"));
    let repository = root.join("owner").join(name);
    std::fs::create_dir_all(&repository).unwrap();
    git_ok(&repository, &["init", "--quiet", "-b", "main"], &[]);
    git_ok(&repository, &["config", "user.name", "fixture"], &[]);
    git_ok(
        &repository,
        &["config", "user.email", "fixture@example.com"],
        &[],
    );
    git_ok(&repository, &["config", "core.autocrlf", "false"], &[]);

    let mut commits = Vec::new();

    write(&repository, "a.txt", b"one\ntwo\n");
    git_ok(&repository, &["add", "."], &[]);
    commits.push((
        "root",
        commit(
            &repository,
            "Initial commit",
            ALICE,
            "2024-01-01T00:00:00Z",
            &[],
        ),
    ));

    write(&repository, "a.txt", b"two\nthree\n");
    write(&repository, "dir/b.txt", b"alpha\nbeta\ngamma\n");
    write(&repository, "bin.dat", &[0, 1, 2, 0, 255, 254, 0]);
    git_ok(&repository, &["add", "."], &[]);
    commits.push((
        "modify",
        commit(
            &repository,
            "Modify and add files",
            ALICE_WORK,
            "2024-01-02T00:00:00Z",
            &[],
        ),
    ));

    git_ok(&repository, &["checkout", "--quiet", "-b", "feature"], &[]);
    git_ok(&repository, &["mv", "dir/b.txt", "dir/c.txt"], &[]);
    commits.push((
        "rename",
        commit(
            &repository,
            "Rename b to c\n\nBody paragraph one.\n\n  Indented second paragraph.",
            BOB,
            "2024-01-03T00:00:00Z",
            &[],
        ),
    ));
    write(&repository, "tie1.txt", b"t1\n");
    git_ok(&repository, &["add", "."], &[]);
    commits.push((
        "tie1",
        commit(&repository, "Tie one", BOB, "2024-01-04T00:00:00Z", &[]),
    ));
    write(&repository, "tie2.txt", b"t2\n");
    git_ok(&repository, &["add", "."], &[]);
    commits.push((
        "tie2",
        commit(&repository, "Tie two", BOB, "2024-01-04T00:00:00Z", &[]),
    ));

    git_ok(&repository, &["checkout", "--quiet", "main"], &[]);
    git_ok(&repository, &["rm", "--quiet", "a.txt"], &[]);
    commits.push((
        "delete",
        commit(&repository, "Delete a", ALICE, "2024-01-05T00:00:00Z", &[]),
    ));
    commits.push((
        "merge",
        merge(&repository, "feature", ZOE, "2024-01-06T00:00:00Z"),
    ));

    Fixture {
        root,
        repository,
        commits,
    }
}

/// Clones `fixture` as a bare repository, like the service does.
pub fn bare_clone(fixture: &Fixture) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(unique_name("git-api-bare"));
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("clone");
    // Forward slashes: the repository name is the last `/`-separated component.
    let source = fixture.repository.to_string_lossy().replace('\\', "/");
    git_ok(
        &root,
        &[
            "clone",
            "--quiet",
            "--bare",
            source.as_str(),
            target.to_str().unwrap(),
        ],
        &[],
    );
    (root, target)
}

/// Environment that rewrites `https://github.com/` to the fixture root, so the
/// real service can "clone" without network access.
pub fn rewrite_env(fixture: &Fixture) -> Vec<(&'static str, Option<String>)> {
    vec![
        ("GIT_CONFIG_COUNT", Some("1".to_owned())),
        (
            "GIT_CONFIG_KEY_0",
            Some(format!("url.{}.insteadOf", fixture.base_url())),
        ),
        ("GIT_CONFIG_VALUE_0", Some("https://github.com/".to_owned())),
        ("GIT_ALLOWED_HOSTS", Some("github.com".to_owned())),
        ("GIT_API_TOKEN", None),
    ]
}

/// Clears read-only flags below `path`. Git marks objects read-only and local
/// clones hard-link them; the legacy Haskell cleanup cannot delete read-only
/// files on Windows, so the differential test relaxes the fixture.
pub fn make_writable(path: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    let mut permissions = metadata.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    let _ = std::fs::set_permissions(path, permissions);
    if metadata.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                make_writable(&entry.path());
            }
        }
    }
}
