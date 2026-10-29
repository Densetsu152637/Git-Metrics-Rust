//! Repository analysis: validates a request, clones the repository into an
//! isolated per-credential directory, and assembles commit, branch, and
//! contributor metrics from Git CLI output.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::available_parallelism;

use tokio::sync::mpsc::Sender;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::command::{
    delete_directory_if_exists, execute_command, execute_command_timed_out, Command, CommandResult,
};
use crate::git_commands::{
    check_if_repo_exists, check_is_git_directory, clone_repo, get_all_commits_from, get_branches,
    get_commit_details, get_commit_diff, get_contributor_emails, get_repo_name,
};
use crate::parsing::{
    merge_file_metadata, pair_by_file_path, parse_commit_data, parse_commit_hashes,
    parse_contributor_emails, parse_file_data_from_commit, parse_file_data_from_diff,
    parse_repo_branches, parse_repo_exists, parse_repo_name, parsed, successful, user_error,
    ParseResult,
};
use crate::security::{
    configured_allowed_hosts, repository_access_path, validate_auth_token, validate_repository_url,
    RepositoryTarget,
};
use crate::text::unique;
use crate::threading::{
    emit, pass_all_async, pass_through_async, safe_print, AwaitBatch, BatchAborted, WorkerPool,
};
use crate::types::{BranchData, CommitData, ContributorData, RepositoryData};

const CLONE_TIMEOUT_SECONDS: u64 = 10;
const COMMAND_POOL_MULTIPLIER: usize = 4;

type Progress = Sender<String>;
type DirectoryLock = (Arc<AsyncMutex<()>>, usize);
type SharedResult = Result<Arc<RepositoryData>, String>;

// ---------------------------------------------------------------------------
// Directory lifetime
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DirectoryState {
    refcounts: Mutex<HashMap<PathBuf, usize>>,
    locks: Mutex<HashMap<PathBuf, DirectoryLock>>,
}

/// Per-directory locks and reference counts. A directory is created by the
/// first user and deleted when its last user releases it.
#[derive(Clone, Default)]
pub struct DirectoryRegistry {
    state: Arc<DirectoryState>,
}

/// Counts a lock user so the registry entry lives while anyone holds or waits
/// for it, even if the waiter is cancelled.
struct Registration {
    state: Arc<DirectoryState>,
    path: PathBuf,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut locks = self.state.locks.lock().expect("directory locks poisoned");
        if let Some((_, users)) = locks.get_mut(&self.path) {
            *users -= 1;
            if *users == 0 {
                locks.remove(&self.path);
            }
        }
    }
}

struct LockLease {
    _guard: OwnedMutexGuard<()>,
    _registration: Registration,
}

/// Removes a half-initialised directory if initialisation is cancelled.
struct CleanupOnDrop {
    path: Option<PathBuf>,
}

impl Drop for CleanupOnDrop {
    fn drop(&mut self) {
        if let (Some(path), Ok(runtime)) = (self.path.take(), tokio::runtime::Handle::try_current())
        {
            runtime.spawn(async move {
                if let Err(error) = delete_directory_if_exists(&path, || {}).await {
                    safe_print(&error);
                }
            });
        }
    }
}

impl DirectoryRegistry {
    async fn lock(&self, path: &Path) -> LockLease {
        let mutex = {
            let mut locks = self.state.locks.lock().expect("directory locks poisoned");
            let entry = locks
                .entry(path.to_path_buf())
                .or_insert_with(|| (Arc::new(AsyncMutex::new(())), 0));
            entry.1 += 1;
            Arc::clone(&entry.0)
        };
        let registration = Registration {
            state: Arc::clone(&self.state),
            path: path.to_path_buf(),
        };
        LockLease {
            _guard: mutex.lock_owned().await,
            _registration: registration,
        }
    }

    /// Creates `path` and runs `initialize` when this is its first user.
    /// Later users only take a reference. Returns whether this call created it.
    pub async fn create<F>(&self, path: &Path, initialize: F) -> Result<bool, String>
    where
        F: std::future::Future<Output = Result<(), String>>,
    {
        let _lock = self.lock(path).await;
        let first_user = {
            let mut counts = self.state.refcounts.lock().expect("refcounts poisoned");
            match counts.get_mut(path) {
                None => true,
                Some(count) => {
                    *count += 1;
                    false
                }
            }
        };
        if !first_user {
            return Ok(false);
        }

        let mut cleanup = CleanupOnDrop {
            path: Some(path.to_path_buf()),
        };
        let outcome = async {
            tokio::fs::create_dir_all(path)
                .await
                .map_err(|error| error.to_string())?;
            initialize.await
        }
        .await;
        cleanup.path = None;

        match outcome {
            Ok(()) => {
                // Publish while nothing can cancel us, so initialised disk state
                // always has its reference count.
                self.state
                    .refcounts
                    .lock()
                    .expect("refcounts poisoned")
                    .insert(path.to_path_buf(), 1);
                Ok(true)
            }
            Err(error) => match delete_directory_if_exists(path, || {}).await {
                Ok(_) => Err(error),
                Err(cleanup_error) => Err(user_error(&cleanup_error)),
            },
        }
    }

    /// Drops one reference to `path`, deleting it (after running `before`) when
    /// the last reference goes.
    pub async fn release(&self, path: &Path, before: impl FnOnce()) -> Result<bool, String> {
        let _lock = self.lock(path).await;
        let delete = {
            let mut counts = self.state.refcounts.lock().expect("refcounts poisoned");
            match counts.get(path).copied() {
                Some(1) => {
                    counts.remove(path);
                    true
                }
                Some(count) if count > 1 => {
                    counts.insert(path.to_path_buf(), count - 1);
                    false
                }
                _ => false,
            }
        };
        if delete {
            delete_directory_if_exists(path, before).await?;
        }
        Ok(delete)
    }
}

/// A reference to an initialised directory. Release it explicitly to observe
/// cleanup errors; if dropped first (cancellation), release runs in the background.
struct DirectoryLease {
    registry: DirectoryRegistry,
    path: PathBuf,
    progress: Progress,
    armed: bool,
}

impl DirectoryLease {
    async fn release(mut self) -> Result<bool, String> {
        self.armed = false;
        let progress = self.progress.clone();
        self.registry
            .release(&self.path, move || {
                emit(&progress, "Cleaning Up Directory...")
            })
            .await
    }
}

impl Drop for DirectoryLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (registry, path, progress) = (
                self.registry.clone(),
                self.path.clone(),
                self.progress.clone(),
            );
            runtime.spawn(async move {
                let released = registry
                    .release(&path, move || emit(&progress, "Cleaning Up Directory..."))
                    .await;
                if let Err(error) = released {
                    safe_print(&error);
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Repository service
// ---------------------------------------------------------------------------

/// Shared analysis service. Concurrent requests for the same credential-scoped
/// repository share one analysis; the first caller (the leader) owns progress.
pub struct RepositoryService {
    parsing_pool: WorkerPool,
    command_pool: WorkerPool,
    requests: AwaitBatch<PathBuf, SharedResult>,
    directories: DirectoryRegistry,
}

impl Default for RepositoryService {
    fn default() -> Self {
        Self::new()
    }
}

impl RepositoryService {
    /// Creates the service with `N` parsing workers and `4N` command workers
    /// for `N` logical cores.
    pub fn new() -> Self {
        let cores = available_parallelism().map_or(1, usize::from);
        Self {
            parsing_pool: WorkerPool::new(cores, "Parsing Pool"),
            command_pool: WorkerPool::new(cores * COMMAND_POOL_MULTIPLIER, "Command Pool"),
            requests: AwaitBatch::new(),
            directories: DirectoryRegistry::default(),
        }
    }

    pub fn parsing_workers(&self) -> usize {
        self.parsing_pool.num_workers()
    }

    pub fn command_workers(&self) -> usize {
        self.command_pool.num_workers()
    }

    /// Validates the request and returns the repository's metrics. Validation
    /// failures are reported through `progress` and the returned error; the
    /// token error takes precedence over the URL error.
    pub async fn analyze(
        &self,
        raw_url: String,
        raw_auth_token: Option<String>,
        progress: Progress,
    ) -> Result<Arc<RepositoryData>, String> {
        let allowed_hosts = configured_allowed_hosts();
        let auth_token = validate_auth_token(raw_auth_token.as_deref())
            .inspect_err(|error| emit(&progress, error.clone()))?;
        let target = validate_repository_url(&allowed_hosts, &raw_url)
            .inspect_err(|error| emit(&progress, error.clone()))?;

        let key = repository_access_path(auth_token.as_deref(), &target);
        let shared = self
            .requests
            .run(key, || self.fetch_unbatched(target, auth_token, progress))
            .await;
        match shared {
            Ok(result) => result,
            Err(BatchAborted) => {
                Err("The shared repository request was cancelled before it finished.".to_owned())
            }
        }
    }

    async fn fetch_unbatched(
        &self,
        target: RepositoryTarget,
        auth_token: Option<String>,
        progress: Progress,
    ) -> SharedResult {
        match self
            .fetch_checked(&target, auth_token.as_deref(), &progress)
            .await
        {
            Ok(data) => Ok(data),
            Err(text) => {
                let message = format!("Encountered error:\n{text}");
                emit(&progress, message.clone());
                safe_print(&message);
                Err(text)
            }
        }
    }

    async fn fetch_checked(
        &self,
        target: &RepositoryTarget,
        auth_token: Option<&str>,
        progress: &Progress,
    ) -> SharedResult {
        let working_directory = std::env::temp_dir();
        let clone_root = working_directory.join("cloned-repos");
        let repository_path = clone_root.join(repository_access_path(auth_token, target));
        let url = target.repository_url.as_str();

        tokio::fs::create_dir_all(&clone_root)
            .await
            .map_err(|error| error.to_string())?;
        emit(progress, "Validating repo exists...");

        self.exec_and_parse(
            &working_directory,
            check_if_repo_exists(url, auth_token),
            parse_repo_exists,
            "Repo does not exist",
        )
        .await?;
        emit(progress, "Found the repo!");

        let clone = async {
            emit(progress, "Cloning repo...");
            let cloned = execute_command_timed_out(
                CLONE_TIMEOUT_SECONDS,
                &clone_root,
                &clone_repo(url, &repository_path, auth_token),
            )
            .await?;
            parsed("Failed to clone the repo", successful(&cloned))?;

            let checked =
                execute_command(&repository_path, &check_is_git_directory(&repository_path))
                    .await?;
            parsed("Failed to initialise the filepath", successful(&checked))?;
            Ok(())
        };
        self.directories.create(&repository_path, clone).await?;
        let lease = DirectoryLease {
            registry: self.directories.clone(),
            path: repository_path.clone(),
            progress: progress.clone(),
            armed: true,
        };

        let processing = async {
            emit(progress, "Getting repository data...");
            let data = self.formulate_repo_data(&repository_path, progress).await?;
            emit(progress, "Data processed!");
            Ok(Arc::new(data))
        }
        .await;
        // A cleanup failure replaces the processing outcome and stays visible.
        lease.release().await.map_err(|error| user_error(&error))?;
        processing
    }

    async fn exec_and_parse<T>(
        &self,
        directory: &Path,
        command: Command,
        parser: fn(&str) -> ParseResult<T>,
        message: &'static str,
    ) -> Result<T, String>
    where
        T: Clone + Send + Sync + 'static,
    {
        let directory = directory.to_path_buf();
        pass_through_async(
            &self.command_pool,
            &self.parsing_pool,
            move |command: Command| {
                let directory = directory.clone();
                async move { execute_command(&directory, &command).await }
            },
            move |result: Arc<CommandResult>| {
                let output = parsed(message, successful(&result))?;
                parsed(message, parser(output))
            },
            command,
        )
        .await
    }

    async fn exec_and_parse_all<T>(
        &self,
        directory: &Path,
        commands: Vec<Command>,
        parser: fn(&str) -> ParseResult<T>,
        message: &'static str,
    ) -> Result<Vec<T>, String>
    where
        T: Clone + Send + Sync + 'static,
    {
        let directory = directory.to_path_buf();
        pass_all_async(
            &self.command_pool,
            &self.parsing_pool,
            move |command: Command| {
                let directory = directory.clone();
                async move { execute_command(&directory, &command).await }
            },
            move |result: Arc<CommandResult>| {
                let output = parsed(message, successful(&result))?;
                parsed(message, parser(output))
            },
            commands,
        )
        .await
    }

    /// Parses, transforms, and assembles the data of an existing clone.
    pub async fn formulate_repo_data(
        &self,
        path: &Path,
        progress: &Progress,
    ) -> Result<RepositoryData, String> {
        emit(progress, "Searching for branch names...");
        let collected_branches = self
            .exec_and_parse(
                path,
                get_branches(),
                parse_repo_branches,
                "Failed to parse git branch names",
            )
            .await?;
        let branch_names = unique(collected_branches);
        let filtered_branch_names: Vec<String> = branch_names
            .iter()
            .map(|name| strip_first(&strip_first(name, "origin/"), "remotes/"))
            .collect();

        emit(progress, "Searching for commit hashes...");
        let hashes_per_branch = self
            .exec_and_parse_all(
                path,
                branch_names
                    .iter()
                    .map(|branch| get_all_commits_from(branch))
                    .collect(),
                parse_commit_hashes,
                "Failed to parse commit hashes from branches",
            )
            .await?;
        let all_commit_hashes = unique(hashes_per_branch.iter().flatten().cloned());
        let commits_found = all_commit_hashes.len();

        emit(progress, "Formulating all commit data...");
        let all_commit_data = self
            .formulate_commits(path, all_commit_hashes, commits_found, progress)
            .await?;

        emit(progress, "Formulating all contributors...");
        let unique_names = unique(
            all_commit_data
                .iter()
                .map(|commit| commit.contributor_name.clone()),
        );
        let emails_per_name = self
            .exec_and_parse_all(
                path,
                unique_names
                    .iter()
                    .map(|name| get_contributor_emails(name))
                    .collect(),
                parse_contributor_emails,
                "Failed to formulate contributor emails",
            )
            .await?;
        let contributors: BTreeMap<String, ContributorData> = unique_names
            .into_iter()
            .zip(emails_per_name)
            .map(|(name, emails)| (name.clone(), ContributorData { name, emails }))
            .collect();
        let commits: BTreeMap<String, CommitData> = all_commit_data
            .into_iter()
            .map(|commit| (commit.commit_hash.clone(), commit))
            .collect();

        emit(progress, "Linking branches to their commits...");
        let branch_to_commits: HashMap<&str, &Vec<String>> = filtered_branch_names
            .iter()
            .map(String::as_str)
            .zip(&hashes_per_branch)
            .collect();

        emit(progress, "Formulating all branch data...");
        let timestamp_for = |hash: &String| {
            commits
                .get(hash)
                .map_or("", |commit| commit.timestamp.as_str())
        };
        let branches = filtered_branch_names
            .iter()
            .map(|branch| {
                let mut hashes = branch_to_commits
                    .get(branch.as_str())
                    .map(|hashes| (*hashes).clone())
                    .unwrap_or_default();
                // Newest first by timestamp text; ties keep Git's order.
                hashes.sort_by(|left, right| timestamp_for(right).cmp(timestamp_for(left)));
                BranchData {
                    branch_name: branch.clone(),
                    commit_hashes: hashes,
                }
            })
            .collect();

        emit(progress, "Fetching repo name...");
        let repo_name = self
            .exec_and_parse(
                path,
                get_repo_name(),
                parse_repo_name,
                "Failed to retrieve repo name",
            )
            .await?;

        Ok(RepositoryData {
            repo_name,
            branches,
            all_commits: commits,
            contributors,
        })
    }

    async fn formulate_commits(
        &self,
        path: &Path,
        hashes: Vec<String>,
        commits_found: usize,
        progress: &Progress,
    ) -> Result<Vec<CommitData>, String> {
        const MESSAGE: &str = "Failed to formulate all commit data";
        let directory = path.to_path_buf();
        let counter = Arc::new(AtomicUsize::new(0));
        let progress = progress.clone();
        pass_all_async(
            &self.command_pool,
            &self.parsing_pool,
            move |(details, diff): (Command, Command)| {
                let directory = directory.clone();
                async move {
                    let details = execute_command(&directory, &details).await?;
                    let diff = execute_command(&directory, &diff).await?;
                    Ok((details, diff))
                }
            },
            move |results: Arc<(CommandResult, CommandResult)>| {
                let (details, diff) = (&results.0, &results.1);
                let raw_details = parsed(MESSAGE, successful(details))?;
                let raw_diff = parsed(MESSAGE, successful(diff))?;
                let commit = parsed(MESSAGE, parse_commit_data(raw_details))?;
                let file_info = commit
                    .involved_files
                    .iter()
                    .map(|line| parsed(MESSAGE, parse_file_data_from_commit(line)))
                    .collect::<Result<Vec<_>, _>>()?;
                let file_diffs = parsed(MESSAGE, parse_file_data_from_diff(raw_diff))?;
                let file_data = merge_file_metadata(pair_by_file_path(&file_info, &file_diffs));

                let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
                emit(
                    &progress,
                    format!("Formulating all commit data ({count}/{commits_found})..."),
                );
                Ok(CommitData {
                    commit_hash: commit.commit_hash,
                    commit_title: commit.commit_title,
                    contributor_name: commit.contributor_name,
                    description: commit.description,
                    timestamp: commit.timestamp,
                    file_data,
                })
            },
            hashes
                .iter()
                .map(|hash| (get_commit_details(hash), get_commit_diff(hash)))
                .collect(),
        )
        .await
    }
}

/// Replaces the first occurrence of `needle` anywhere in `text` with nothing.
fn strip_first(text: &str, needle: &str) -> String {
    text.replacen(needle, "", 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_prefixes_are_removed_once_in_source_order() {
        let filter = |name: &str| strip_first(&strip_first(name, "origin/"), "remotes/");
        assert_eq!(filter("origin/main"), "main");
        assert_eq!(filter("remotes/origin/main"), "main");
        assert_eq!(filter("feature/origin/x"), "feature/x");
        assert_eq!(filter("origin/origin/x"), "origin/x");
        assert_eq!(filter("plain"), "plain");
    }

    #[tokio::test]
    async fn directories_are_created_once_and_deleted_with_their_last_user() {
        let registry = DirectoryRegistry::default();
        let path = std::env::temp_dir().join(format!("git-api-registry-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&path).await;

        assert!(registry.create(&path, async { Ok(()) }).await.unwrap());
        assert!(!registry
            .create(&path, async { Err("not run".to_owned()) })
            .await
            .unwrap());
        assert!(path.is_dir());

        assert!(!registry.release(&path, || {}).await.unwrap());
        assert!(path.is_dir());
        let mut before_ran = false;
        assert!(registry.release(&path, || before_ran = true).await.unwrap());
        assert!(before_ran);
        assert!(!path.exists());
        assert!(!registry.release(&path, || {}).await.unwrap());
    }

    #[tokio::test]
    async fn failed_initialisation_removes_the_directory_and_is_retryable() {
        let registry = DirectoryRegistry::default();
        let path =
            std::env::temp_dir().join(format!("git-api-registry-fail-{}", std::process::id()));
        let failed = registry
            .create(&path, async { Err("clone failed".to_owned()) })
            .await;
        assert_eq!(failed, Err("clone failed".to_owned()));
        assert!(!path.exists());
        assert!(registry.create(&path, async { Ok(()) }).await.unwrap());
        registry.release(&path, || {}).await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_initialisation_cleans_up_in_the_background() {
        let registry = DirectoryRegistry::default();
        let path =
            std::env::temp_dir().join(format!("git-api-registry-cancel-{}", std::process::id()));
        let creating = {
            let (registry, path) = (registry.clone(), path.clone());
            tokio::spawn(async move {
                registry
                    .create(&path, std::future::pending::<Result<(), String>>())
                    .await
            })
        };
        while !path.exists() {
            tokio::task::yield_now().await;
        }
        creating.abort();
        let _ = creating.await;
        for _ in 0..200 {
            if !path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(!path.exists());
        // The key is usable again.
        assert!(registry.create(&path, async { Ok(()) }).await.unwrap());
        registry.release(&path, || {}).await.unwrap();
    }
}
