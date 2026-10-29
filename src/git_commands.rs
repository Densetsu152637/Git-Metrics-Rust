//! Git command argument vectors. Repository-controlled values only ever appear
//! as discrete arguments, never in a shell string, and tokens only in the
//! child environment.

use std::path::Path;

use crate::command::{Command, FailMessage};

/// Literal separator (backslash, `n`, then `|||END|||`) between commit fields.
pub const DELIM: &str = "\\n|||END|||";

const CREDENTIAL_HELPER: &str = r#"!f() { if test "$1" = get; then printf '%s\n' 'username=x-access-token' "password=$GIT_AUTH_TOKEN"; fi; }; f"#;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn git(arguments: Vec<String>) -> Command {
    Command::new("git", arguments)
}

/// Supplies an optional token through a one-shot credential helper. The raw
/// token is held in the child environment and never appears in the Git URL or
/// argument vector. Clearing inherited helpers prevents the service from using
/// machine-wide credentials for otherwise public requests.
fn authenticated_git_command(auth_token: Option<&str>, extra: Vec<String>) -> Command {
    let mut arguments = match auth_token {
        None => strings(&["-c", "credential.helper=", "-c", "core.askPass=true"]),
        Some(_) => {
            let mut arguments = strings(&["-c", "credential.helper=", "-c"]);
            arguments.push(format!("credential.helper={CREDENTIAL_HELPER}"));
            arguments.extend(strings(&[
                "-c",
                "credential.useHttpPath=true",
                "-c",
                "core.askPass=true",
            ]));
            arguments
        }
    };
    arguments.extend(extra);

    let mut environment = vec![("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned())];
    if let Some(token) = auth_token {
        environment.push(("GIT_AUTH_TOKEN".to_owned(), token.to_owned()));
    }
    let mut command = git(arguments);
    command.env_vars = Some(environment);
    command.env_clean = strings(&["GIT_ASKPASS", "GCM_INTERACTIVE", "GIT_AUTH_TOKEN"]);
    command
}

pub fn check_if_repo_exists(url: &str, auth_token: Option<&str>) -> Command {
    let mut command = authenticated_git_command(auth_token, strings(&["ls-remote", url]));
    command.on_fail = FailMessage::RepositoryProbe(url.to_owned());
    command
}

pub fn clone_repo(url: &str, target_directory: &Path, auth_token: Option<&str>) -> Command {
    let mut command = authenticated_git_command(
        auth_token,
        vec![
            "clone".to_owned(),
            "--bare".to_owned(),
            url.to_owned(),
            target_directory.to_string_lossy().into_owned(),
        ],
    );
    command.on_fail = FailMessage::Clone;
    command
}

pub fn check_is_git_directory(directory: &Path) -> Command {
    git(vec![
        "-C".to_owned(),
        directory.to_string_lossy().into_owned(),
        "rev-parse".to_owned(),
        "--git-dir".to_owned(),
    ])
}

pub fn get_branches() -> Command {
    git(strings(&[
        "--no-pager",
        "branch",
        "-a",
        "--format=%(refname:short)",
    ]))
}

pub fn get_all_commits_from(branch: &str) -> Command {
    git(vec![
        "--no-pager".to_owned(),
        "log".to_owned(),
        "--format=%H".to_owned(),
        "--end-of-options".to_owned(),
        branch.to_owned(),
        "--".to_owned(),
    ])
}

pub fn get_contributor_emails(name: &str) -> Command {
    git(vec![
        "log".to_owned(),
        format!("--author={name}"),
        "--pretty=format:%ae".to_owned(),
        "--".to_owned(),
    ])
}

pub fn get_commit_details(hash: &str) -> Command {
    let format = ["%H", "%an", "%cI", "%s", "%b"].iter().fold(
        String::from("--pretty=format:"),
        |mut format, field| {
            format.push_str(field);
            format.push_str(DELIM);
            format
        },
    );
    git(vec![
        "show".to_owned(),
        format,
        "--name-status".to_owned(),
        hash.to_owned(),
        "--".to_owned(),
    ])
}

pub fn get_commit_diff(hash: &str) -> Command {
    git(vec![
        "--no-pager".to_owned(),
        "diff-tree".to_owned(),
        "-p".to_owned(),
        hash.to_owned(),
        "--".to_owned(),
    ])
}

pub fn get_repo_name() -> Command {
    git(strings(&["remote", "get-url", "origin"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "private-repository-secret";

    fn env_value<'a>(command: &'a Command, name: &str) -> Option<&'a str> {
        command
            .env_vars
            .as_ref()?
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn tokens_stay_out_of_arguments_and_live_in_the_environment() {
        let probe = check_if_repo_exists("https://github.com/o/r", Some(SECRET));
        let clone = clone_repo(
            "https://github.com/o/r",
            Path::new("clone-target"),
            Some(SECRET),
        );
        assert!(probe
            .arguments
            .iter()
            .chain(&clone.arguments)
            .all(|argument| !argument.contains(SECRET)));
        assert_eq!(env_value(&probe, "GIT_AUTH_TOKEN"), Some(SECRET));
        assert_eq!(env_value(&probe, "GIT_TERMINAL_PROMPT"), Some("0"));
        assert!(probe
            .arguments
            .iter()
            .any(|argument| argument.contains("credential.helper=!")));
        assert!(probe
            .arguments
            .contains(&"credential.useHttpPath=true".to_owned()));
        assert_eq!(
            probe.arguments[..4],
            [
                "-c",
                "credential.helper=",
                "-c",
                probe.arguments[3].as_str()
            ]
        );
        assert_eq!(
            probe.env_clean,
            ["GIT_ASKPASS", "GCM_INTERACTIVE", "GIT_AUTH_TOKEN"]
        );
    }

    #[test]
    fn public_requests_do_not_inherit_tokens_or_helpers() {
        let probe = check_if_repo_exists("https://github.com/o/r", None);
        assert_eq!(env_value(&probe, "GIT_AUTH_TOKEN"), None);
        assert!(probe.env_clean.contains(&"GIT_AUTH_TOKEN".to_owned()));
        assert_eq!(
            probe.arguments,
            [
                "-c",
                "credential.helper=",
                "-c",
                "core.askPass=true",
                "ls-remote",
                "https://github.com/o/r"
            ]
        );
    }

    #[test]
    fn clone_is_bare_into_the_target_directory() {
        let clone = clone_repo("https://github.com/o/r", Path::new("target"), None);
        assert_eq!(
            clone.arguments[4..],
            ["clone", "--bare", "https://github.com/o/r", "target"]
        );
        assert_eq!(clone.executable, "git");
    }

    #[test]
    fn repository_controlled_values_are_discrete_arguments() {
        let name = "teacher\"; touch /tmp/pwned; \"";
        let author = get_contributor_emails(name);
        assert_eq!(author.executable, "git");
        assert!(author.arguments.contains(&format!("--author={name}")));
        assert_eq!(author.arguments.last().map(String::as_str), Some("--"));

        let branch = get_all_commits_from("--exec-path=malicious");
        assert_eq!(
            branch.arguments[..4],
            ["--no-pager", "log", "--format=%H", "--end-of-options"]
        );
        assert_eq!(branch.arguments[4], "--exec-path=malicious");
        assert_eq!(branch.arguments[5], "--");
    }

    #[test]
    fn commit_commands_use_the_literal_delimiter() {
        let details = get_commit_details("abc");
        assert_eq!(
            details.arguments[1],
            "--pretty=format:%H\\n|||END|||%an\\n|||END|||%cI\\n|||END|||%s\\n|||END|||%b\\n|||END|||"
        );
        assert_eq!(details.arguments[2..], ["--name-status", "abc", "--"]);
        assert_eq!(
            get_commit_diff("abc").arguments,
            ["--no-pager", "diff-tree", "-p", "abc", "--"]
        );
        assert_eq!(get_repo_name().arguments, ["remote", "get-url", "origin"]);
        assert_eq!(
            get_branches().arguments,
            ["--no-pager", "branch", "-a", "--format=%(refname:short)"]
        );
    }
}
