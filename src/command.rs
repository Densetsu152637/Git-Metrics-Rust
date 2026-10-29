//! Subprocess execution and directory helpers.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command as ProcessCommand;

use crate::text::show_string;

const DELETE_ATTEMPTS: u32 = 3;
const DELETE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How a non-zero exit is described, given the raw command text and stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailMessage {
    Default,
    RepositoryProbe(String),
    Clone,
}

impl FailMessage {
    fn render(&self, raw_command: &str, stderr: &str) -> String {
        match self {
            Self::Default => format!("Command:\n{raw_command}\nError:\n{stderr}"),
            Self::RepositoryProbe(url) => format!("Could not find Repo ({url}): {stderr}"),
            Self::Clone => {
                format!("Error cloning repo:\nCommand:\n{raw_command}\nError Message:\n{stderr}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub executable: String,
    pub arguments: Vec<String>,
    /// Extra child environment. `None` inherits the service environment as-is;
    /// `Some` also removes every name in `env_clean` first.
    pub env_vars: Option<Vec<(String, String)>>,
    pub env_clean: Vec<String>,
    pub on_fail: FailMessage,
}

impl Command {
    pub fn new(executable: &str, arguments: Vec<String>) -> Self {
        Self {
            executable: executable.to_owned(),
            arguments,
            env_vars: None,
            env_clean: Vec::new(),
            on_fail: FailMessage::Default,
        }
    }

    /// Command text used in diagnostics: arguments are rendered like Haskell's `show`.
    pub fn raw_text(&self) -> String {
        std::iter::once(self.executable.clone())
            .chain(self.arguments.iter().map(|argument| show_string(argument)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub result: String,
    pub error_msg: Option<String>,
    pub std_error: Option<String>,
}

impl CommandResult {
    fn failed(message: String) -> Self {
        Self {
            result: String::new(),
            error_msg: Some(message),
            std_error: None,
        }
    }
}

async fn read_all<R: AsyncRead + Unpin>(mut reader: R) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer).await?;
    Ok(buffer)
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    status.code().unwrap_or(-1)
}

/// Runs a command in `directory`. The child is killed and reaped by Tokio if
/// this future is dropped (cancellation, timeout, or a failed sibling task).
/// Errors are reserved for failures to start or read the process.
pub async fn execute_command(directory: &Path, command: &Command) -> Result<CommandResult, String> {
    let is_directory = tokio::fs::metadata(directory)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    if !is_directory {
        return Ok(CommandResult::failed(format!(
            "Invalid filepath: {}",
            directory.display()
        )));
    }

    let raw_command = command.raw_text();
    let mut process = ProcessCommand::new(&command.executable);
    process
        .args(&command.arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(extra) = &command.env_vars {
        for name in &command.env_clean {
            process.env_remove(name);
        }
        for (name, value) in extra {
            process.env(name, value);
        }
    }

    let mut child = process
        .spawn()
        .map_err(|error| format!("{}: failed to start process ({error})", command.executable))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err("executeCommand: failed to create stdout/stderr pipes".to_owned());
    };

    // Drain both pipes concurrently. Reading either one first can deadlock when
    // the child fills the other pipe's OS buffer.
    let (raw_stdout, raw_stderr) = tokio::join!(read_all(stdout), read_all(stderr));
    let stdout_text =
        String::from_utf8_lossy(&raw_stdout.map_err(|error| error.to_string())?).into_owned();
    let stderr_text =
        String::from_utf8_lossy(&raw_stderr.map_err(|error| error.to_string())?).into_owned();
    let status = child.wait().await.map_err(|error| error.to_string())?;

    let std_error = (!stderr_text.is_empty()).then(|| stderr_text.clone());
    if status.success() {
        return Ok(CommandResult {
            result: stdout_text,
            error_msg: None,
            std_error,
        });
    }
    let message = format!(
        "Process exited with code {} from path: {}:\n{}",
        exit_code(status),
        directory.display(),
        command.on_fail.render(&raw_command, &stderr_text)
    );
    Ok(CommandResult {
        result: stdout_text,
        error_msg: Some(message),
        std_error,
    })
}

/// [`execute_command`] with a wall-clock limit; expiry kills the child.
pub async fn execute_command_timed_out(
    seconds: u64,
    directory: &Path,
    command: &Command,
) -> Result<CommandResult, String> {
    match tokio::time::timeout(
        Duration::from_secs(seconds),
        execute_command(directory, command),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Ok(CommandResult::failed(format!(
            "Process timed out after {}μs for command: {} in path: {}",
            seconds * 1_000_000,
            command.raw_text(),
            directory.display()
        ))),
    }
}

/// Deletes `directory` if it exists, running `before` first. Deletion is
/// retried; the final failure is returned so cleanup errors stay visible.
pub async fn delete_directory_if_exists(
    directory: &Path,
    before: impl FnOnce(),
) -> Result<bool, String> {
    delete_directory_with_retries(directory, before, DELETE_ATTEMPTS, DELETE_RETRY_DELAY).await
}

pub async fn delete_directory_with_retries(
    directory: &Path,
    before: impl FnOnce(),
    attempts: u32,
    delay: Duration,
) -> Result<bool, String> {
    let exists = tokio::fs::metadata(directory)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    if !exists {
        return Ok(false);
    }
    before();
    let mut previous = None;
    for _ in 0..attempts {
        match tokio::fs::remove_dir_all(directory).await {
            Ok(()) => return Ok(true),
            Err(error) => {
                tokio::time::sleep(delay).await;
                previous = Some(error);
            }
        }
    }
    let reason = previous.map_or_else(|| "Nothing".to_owned(), |error| format!("Just {error}"));
    Err(format!(
        "Failed to delete directory \"{}\": {reason}",
        directory.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(arguments: &[&str]) -> Command {
        Command::new(
            "git",
            arguments.iter().map(|value| (*value).to_owned()).collect(),
        )
    }

    #[tokio::test]
    async fn reports_invalid_directories() {
        let missing = std::env::temp_dir().join("git-api-no-such-directory-for-test");
        let result = execute_command(&missing, &git(&["--version"]))
            .await
            .unwrap();
        assert_eq!(result.result, "");
        assert!(result.error_msg.unwrap().starts_with("Invalid filepath: "));
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_failures() {
        let directory = std::env::temp_dir();
        let ok = execute_command(&directory, &git(&["--version"]))
            .await
            .unwrap();
        assert!(ok.result.starts_with("git version"));
        assert_eq!(ok.error_msg, None);

        let failed = execute_command(&directory, &git(&["definitely-not-a-git-command"]))
            .await
            .unwrap();
        let message = failed.error_msg.unwrap();
        assert!(
            message.starts_with("Process exited with code "),
            "{message}"
        );
        assert!(message.contains("Command:\ngit \"definitely-not-a-git-command\"\nError:\n"));
        assert!(failed.std_error.is_some());
    }

    #[tokio::test]
    async fn drains_large_output_on_both_pipes() {
        // `git help -a` writes a long listing; a sequential reader would deadlock
        // if stderr also filled up. This guards the concurrent drain.
        let directory = std::env::temp_dir();
        let result = execute_command(&directory, &git(&["help", "-a"]))
            .await
            .unwrap();
        assert!(result.result.len() > 1000);
    }

    #[tokio::test]
    async fn environment_is_cleaned_and_extended() {
        let directory = std::env::temp_dir();
        let mut command = git(&["config", "--get", "never.set.key"]);
        command.env_vars = Some(vec![("GIT_API_TEST_VALUE".to_owned(), "1".to_owned())]);
        command.env_clean = vec!["GIT_API_TEST_REMOVED".to_owned()];
        let result = execute_command(&directory, &command).await.unwrap();
        // `git config --get` of a missing key exits with 1 and no message.
        assert!(result.error_msg.unwrap().contains("exited with code 1"));
    }

    #[tokio::test]
    async fn spawn_failures_are_errors() {
        let command = Command::new("git-api-no-such-executable", vec![]);
        let error = execute_command(&std::env::temp_dir(), &command)
            .await
            .unwrap_err();
        assert!(error.starts_with("git-api-no-such-executable: failed to start process"));
    }

    #[tokio::test]
    async fn timeout_returns_timeout_result() {
        let command = if cfg!(windows) {
            Command::new("ping", vec!["-n".into(), "30".into(), "127.0.0.1".into()])
        } else {
            Command::new("sleep", vec!["30".into()])
        };
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            execute_command_timed_out(1, &std::env::temp_dir(), &command),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(15));
        let message = result.error_msg.unwrap();
        assert!(
            message.starts_with("Process timed out after 1000000μs for command: "),
            "{message}"
        );
    }

    #[tokio::test]
    async fn directory_deletion_reports_state() {
        let directory =
            std::env::temp_dir().join(format!("git-api-delete-test-{}", std::process::id()));
        assert_eq!(
            delete_directory_if_exists(&directory, || {}).await,
            Ok(false)
        );
        tokio::fs::create_dir_all(directory.join("nested"))
            .await
            .unwrap();
        let mut called = false;
        assert_eq!(
            delete_directory_if_exists(&directory, || called = true).await,
            Ok(true)
        );
        assert!(called);
        assert!(!directory.exists());
    }

    #[test]
    fn failure_messages_match_the_source_formats() {
        let command = git(&["ls-remote", "u"]);
        let raw = command.raw_text();
        assert_eq!(raw, "git \"ls-remote\" \"u\"");
        assert_eq!(
            FailMessage::RepositoryProbe("https://h/o/r".into()).render(&raw, "boom"),
            "Could not find Repo (https://h/o/r): boom"
        );
        assert_eq!(
            FailMessage::Clone.render(&raw, "boom"),
            "Error cloning repo:\nCommand:\ngit \"ls-remote\" \"u\"\nError Message:\nboom"
        );
    }
}
