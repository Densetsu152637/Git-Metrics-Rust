//! Parsers for Git command output. Several parsers deliberately keep the
//! limitations of the original implementation (whitespace-split file lines,
//! last-write-wins pairing by path) because they define the metrics contract.

use std::collections::HashMap;

use crate::command::CommandResult;
use crate::git_commands::DELIM;
use crate::text::{lines, nub, show_string_list, to_lower, trim, trim_start, words};
use crate::types::{ChangeType, FileChanges};

/// A parse outcome; the error is the raw failure text.
pub type ParseResult<T> = Result<T, String>;

const FAILURE_PREFIXES: [&str; 7] = [
    "fatal:",
    "error:",
    "could not",
    "not a git repository",
    "process exited with code",
    "encountered error",
    "process timed out",
];

const LONGEST_FAILURE_PREFIX: usize = 24;

/// Interprets a command result: failures become errors, and stderr output
/// that looks like a failure rejects an otherwise successful run.
pub fn successful(result: &CommandResult) -> ParseResult<&str> {
    match (&result.error_msg, &result.std_error) {
        (Some(error), Some(std_error)) => Err(format!("{error}:\n{std_error}")),
        (Some(error), None) => Err(error.clone()),
        (None, Some(std_error)) => {
            if failed_output(std_error) || failed_output(&result.result) {
                Err(std_error.clone())
            } else {
                Ok(&result.result)
            }
        }
        (None, None) => Ok(&result.result),
    }
}

/// Converts a parse failure into the displayed exception text
/// (`userError`): `user error (<message>:\n<detail>)`.
pub fn parsed<T>(message: &str, result: ParseResult<T>) -> Result<T, String> {
    result.map_err(|error| {
        let text = match (message.is_empty(), error.is_empty()) {
            (true, true) => "Got blank string".to_owned(),
            (true, false) => error,
            (false, _) => format!("{message}:\n{error}"),
        };
        user_error(&text)
    })
}

/// How `ioError (userError text)` is displayed.
pub fn user_error(text: &str) -> String {
    format!("user error ({text})")
}

pub fn failed_output(text: &str) -> bool {
    let prefix: String = text
        .chars()
        .take(LONGEST_FAILURE_PREFIX)
        .map(to_lower)
        .collect();
    FAILURE_PREFIXES
        .iter()
        .any(|failure| prefix.starts_with(failure))
}

pub fn parse_repo_exists(text: &str) -> ParseResult<String> {
    if failed_output(text) {
        Err(text.to_owned())
    } else {
        Ok("repo exists".to_owned())
    }
}

/// Repository name: the last `/`-separated component of the origin URL.
pub fn parse_repo_name(text: &str) -> ParseResult<String> {
    if failed_output(text) {
        return Err(text.to_owned());
    }
    let trimmed = trim(text);
    let cleaned = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    cleaned
        .rsplit('/')
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "Inside parseRepoName failed to get lastElem".to_owned())
}

pub fn parse_contributor_emails(text: &str) -> ParseResult<Vec<String>> {
    if failed_output(text) {
        return Err(text.to_owned());
    }
    Ok(nub(lines(text).into_iter().map(str::to_owned)))
}

pub fn parse_repo_branches(text: &str) -> ParseResult<Vec<String>> {
    if failed_output(text) {
        return Err(text.to_owned());
    }
    Ok(lines(text)
        .into_iter()
        .filter(|line| !line.contains("->"))
        .map(|line| {
            let line = trim(line);
            line.strip_prefix("* ").unwrap_or(line).to_owned()
        })
        .filter(|name| !name.is_empty())
        .collect())
}

pub fn parse_commit_hashes(text: &str) -> ParseResult<Vec<String>> {
    if failed_output(text) {
        return Err(text.to_owned());
    }
    Ok(lines(text)
        .into_iter()
        .filter(|line| !line.starts_with("The system cannot find the path specified."))
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Commit metadata plus the whitespace-split name-status lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InbetweenCommitData {
    pub commit_hash: String,
    pub commit_title: String,
    pub contributor_name: String,
    pub description: String,
    pub timestamp: String,
    pub involved_files: Vec<Vec<String>>,
}

fn trim_block(block: &str) -> String {
    lines(block)
        .into_iter()
        .map(trim_start)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_commit_data(text: &str) -> ParseResult<InbetweenCommitData> {
    if failed_output(text) {
        return Err(text.to_owned());
    }
    let blocks: Vec<&str> = text.split(DELIM).collect();
    if blocks.len() < 6 {
        return Err(format!(
            "has less than 6 blocks: \"{}\" | txt: \"{}\"",
            show_string_list(&blocks),
            text
        ));
    }
    let unlined: String = blocks[5..]
        .iter()
        .flat_map(|block| [*block, "\n"])
        .collect();
    let involved_files = lines(&unlined)
        .into_iter()
        .filter(|line| !line.is_empty())
        .map(|line| words(line).into_iter().map(str::to_owned).collect())
        .collect();
    Ok(InbetweenCommitData {
        commit_hash: trim_block(blocks[0]),
        contributor_name: trim_block(blocks[1]),
        timestamp: trim_block(blocks[2]),
        commit_title: trim_block(blocks[3]),
        description: trim_block(blocks[4]),
        involved_files,
    })
}

/// A parsed `--name-status` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaFileChanges {
    pub filepath: String,
    pub old_file_path: String,
    pub char: ChangeType,
    pub likeness: i64,
}

/// Haskell `readMaybe :: Maybe Int` for the decimal strings Git produces.
fn read_int(text: &str) -> Option<i64> {
    let text = trim(text);
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.chars().all(|value| value.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Parses one whitespace-split name-status line such as `["R100", "old", "new"]`.
pub fn parse_file_data_from_commit(data: &[String]) -> ParseResult<MetaFileChanges> {
    let Some(first) = data.first() else {
        return Err(format!("Malformed dataList: {}", show_string_list(data)));
    };
    let mut first_chars = first.chars();
    let Some(change_char) = first_chars.next() else {
        return Err("Missing change type".to_owned());
    };
    let likeness_text = first_chars.as_str();
    let rest = &data[1..];

    let change = ChangeType::from_char(change_char)
        .ok_or_else(|| format!("Invalid change type: {change_char}"))?;
    let new_path = rest
        .last()
        .ok_or_else(|| "Missing new file path".to_owned())?
        .clone();

    let old_path = match rest.first() {
        Some(old) if matches!(change_char, 'R' | 'C' | 'M') => old.clone(),
        _ => new_path.clone(),
    };
    let likeness = if !rest.is_empty() && matches!(change_char, 'R' | 'C') {
        let cleaned: String = likeness_text
            .chars()
            .filter(|value| *value != ',')
            .collect();
        read_int(&cleaned).unwrap_or(-1)
    } else {
        -1
    };
    Ok(MetaFileChanges {
        filepath: new_path,
        old_file_path: old_path,
        char: change,
        likeness,
    })
}

/// A per-file diff body with its line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaFileChangesDiff {
    pub filepath: String,
    pub diff: Vec<String>,
    pub new_lines: i64,
    pub deleted_lines: i64,
}

const DIFF_HEADER: &str = "diff --git ";

/// Extracts `b/<path>` from `diff --git a/<x> b/<path>` using the last
/// separator that leaves both sides non-empty (POSIX leftmost-longest).
fn extract_file_path(header: &str) -> Option<&str> {
    let after_a = header.strip_prefix("diff --git a/")?;
    after_a
        .rmatch_indices(" b/")
        .find(|(index, _)| *index > 0 && index + 3 < after_a.len())
        .map(|(index, _)| &after_a[index + 3..])
}

/// Splits `git diff-tree -p` output into per-file diffs. Text before the first
/// `diff --git` header is ignored and header lines are excluded from bodies.
pub fn parse_file_data_from_diff(text: &str) -> ParseResult<Vec<MetaFileChangesDiff>> {
    if failed_output(text) {
        return Err(text.to_owned());
    }
    let mut blocks: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut current: Option<(&str, Vec<&str>)> = None;
    for line in lines(text) {
        if line.starts_with(DIFF_HEADER) {
            blocks.extend(current.take());
            current = Some((line, Vec::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    blocks.extend(current);

    Ok(blocks
        .into_iter()
        .map(|(header, body)| {
            let count = |marker: &str, excluded: &str| {
                body.iter()
                    .filter(|line| line.starts_with(marker) && !line.starts_with(excluded))
                    .count() as i64
            };
            MetaFileChangesDiff {
                filepath: extract_file_path(header).unwrap_or("<unknown>").to_owned(),
                new_lines: count("+", "+++"),
                deleted_lines: count("-", "---"),
                diff: body.into_iter().map(str::to_owned).collect(),
            }
        })
        .collect())
}

/// Pairs each name-status record with the diff for its new path (the last diff
/// wins when a path repeats); records without a diff are dropped.
pub fn pair_by_file_path<'a>(
    infos: &'a [MetaFileChanges],
    diffs: &'a [MetaFileChangesDiff],
) -> Vec<(&'a MetaFileChanges, &'a MetaFileChangesDiff)> {
    let by_path: HashMap<&str, &MetaFileChangesDiff> = diffs
        .iter()
        .map(|diff| (diff.filepath.as_str(), diff))
        .collect();
    infos
        .iter()
        .filter_map(|info| {
            by_path
                .get(info.filepath.as_str())
                .map(|diff| (info, *diff))
        })
        .collect()
}

pub fn merge_file_metadata(
    pairs: Vec<(&MetaFileChanges, &MetaFileChangesDiff)>,
) -> Vec<FileChanges> {
    pairs
        .into_iter()
        .map(|(changes, diff)| FileChanges {
            filepath: changes.filepath.clone(),
            old_file_path: changes.old_file_path.clone(),
            char: changes.char,
            likeness: changes.likeness,
            new_lines: diff.new_lines,
            deleted_lines: diff.deleted_lines,
            diff: diff.diff.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn result(output: &str, error: Option<&str>, std_error: Option<&str>) -> CommandResult {
        CommandResult {
            result: output.to_owned(),
            error_msg: error.map(str::to_owned),
            std_error: std_error.map(str::to_owned),
        }
    }

    #[test]
    fn failure_detection_is_a_case_insensitive_prefix_check() {
        assert!(failed_output("FATAL: nope"));
        assert!(failed_output("Could not resolve host"));
        assert!(failed_output("Process exited with code 1"));
        assert!(!failed_output("some fatal: later"));
        assert!(!failed_output(""));
    }

    #[test]
    fn successful_combines_error_and_stderr() {
        assert_eq!(successful(&result("out", None, None)), Ok("out"));
        assert_eq!(
            successful(&result("out", None, Some("warning: x"))),
            Ok("out")
        );
        assert_eq!(
            successful(&result("out", None, Some("fatal: x"))),
            Err("fatal: x".into())
        );
        assert_eq!(
            successful(&result("error: bad", None, Some("hint"))),
            Err("hint".into())
        );
        assert_eq!(
            successful(&result("", Some("boom"), None)),
            Err("boom".into())
        );
        assert_eq!(
            successful(&result("", Some("boom"), Some("detail"))),
            Err("boom:\ndetail".into())
        );
    }

    #[test]
    fn parsed_wraps_errors_like_user_errors() {
        assert_eq!(
            parsed("msg", Err::<(), _>("why".into())),
            Err("user error (msg:\nwhy)".into())
        );
        assert_eq!(
            parsed("", Err::<(), _>(String::new())),
            Err("user error (Got blank string)".into())
        );
        assert_eq!(parsed("msg", Ok(3)), Ok(3));
    }

    #[test]
    fn repo_name_is_the_last_url_component() {
        assert_eq!(
            parse_repo_name("https://github.com/o/Repo.git\n"),
            Ok("Repo".into())
        );
        assert_eq!(
            parse_repo_name("https://github.com/o/Repo"),
            Ok("Repo".into())
        );
        assert_eq!(parse_repo_name(".git"), Ok(String::new()));
        assert!(parse_repo_name("fatal: no remote").is_err());
    }

    #[test]
    fn branches_are_cleaned_and_filtered() {
        let output = "* main\n  feature\n\nHEAD -> origin/main\n  origin/dev \n";
        assert_eq!(
            parse_repo_branches(output),
            Ok(strings(&["main", "feature", "origin/dev"]))
        );
        assert!(parse_repo_branches("fatal: not a git repository").is_err());
    }

    #[test]
    fn commit_hashes_skip_blank_and_platform_noise_lines() {
        let output = "abc\n\nThe system cannot find the path specified.\ndef\n";
        assert_eq!(parse_commit_hashes(output), Ok(strings(&["abc", "def"])));
    }

    #[test]
    fn emails_keep_first_seen_order() {
        assert_eq!(
            parse_contributor_emails("b@x\na@x\nb@x\n"),
            Ok(strings(&["b@x", "a@x"]))
        );
    }

    fn commit_text(files: &str) -> String {
        format!(
            "abc123{d}Ada Lovelace{d}2024-01-02T03:04:05+00:00{d}  Title here{d}  Body line\n   second\n{d}\n\n{files}",
            d = DELIM
        )
    }

    #[test]
    fn commit_data_splits_on_the_literal_delimiter() {
        let parsed = parse_commit_data(&commit_text("M\tsrc/a.rs\nR100\told name\tnew\n")).unwrap();
        assert_eq!(parsed.commit_hash, "abc123");
        assert_eq!(parsed.contributor_name, "Ada Lovelace");
        assert_eq!(parsed.timestamp, "2024-01-02T03:04:05+00:00");
        assert_eq!(parsed.commit_title, "Title here");
        assert_eq!(parsed.description, "Body line\nsecond");
        assert_eq!(
            parsed.involved_files,
            vec![
                strings(&["M", "src/a.rs"]),
                strings(&["R100", "old", "name", "new"])
            ]
        );
    }

    #[test]
    fn commit_data_without_enough_blocks_is_an_error() {
        let error = parse_commit_data("abc\\n|||END|||x").unwrap_err();
        assert!(
            error.starts_with("has less than 6 blocks: \"[\"abc\",\"x\"]\" | txt: \""),
            "{error}"
        );
        assert!(parse_commit_data("fatal: bad object").is_err());
    }

    #[test]
    fn name_status_lines_parse_like_the_source() {
        let parse = |line: &[&str]| parse_file_data_from_commit(&strings(line));
        let renamed = parse(&["R100", "old", "new"]).unwrap();
        assert_eq!(
            (
                renamed.char,
                renamed.likeness,
                renamed.old_file_path.as_str()
            ),
            (ChangeType::R, 100, "old")
        );
        assert_eq!(renamed.filepath, "new");
        let copied = parse(&["C75", "a", "b"]).unwrap();
        assert_eq!((copied.char, copied.likeness), (ChangeType::C, 75));
        let modified = parse(&["M", "p"]).unwrap();
        assert_eq!(
            (modified.old_file_path.as_str(), modified.likeness),
            ("p", -1)
        );
        let added = parse(&["A", "p"]).unwrap();
        assert_eq!((added.old_file_path.as_str(), added.likeness), ("p", -1));
        assert_eq!(parse(&["R", "a", "b"]).unwrap().likeness, -1);
        assert_eq!(parse(&["X", "p"]), Err("Invalid change type: X".into()));
        assert_eq!(parse(&["A"]), Err("Missing new file path".into()));
        assert_eq!(parse(&[]), Err("Malformed dataList: []".into()));
        assert_eq!(parse(&[""]), Err("Missing change type".into()));
    }

    const DIFF: &str = "commitline\n\
diff --git a/old.txt b/new.txt\n\
similarity index 90%\n\
rename from old.txt\n\
rename to new.txt\n\
index 111..222 100644\n\
--- a/old.txt\n\
+++ b/new.txt\n\
@@ -1,2 +1,2 @@\n\
-gone\n\
+added\n\
+++not a header\n\
 context\n\
diff --git a/dir/b.txt b/dir/b.txt\n\
new file mode 100644\n\
+only\n";

    #[test]
    fn diffs_split_per_file_and_count_changed_lines() {
        let diffs = parse_file_data_from_diff(DIFF).unwrap();
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].filepath, "new.txt");
        assert_eq!(diffs[0].diff[0], "similarity index 90%");
        assert_eq!((diffs[0].new_lines, diffs[0].deleted_lines), (1, 1));
        assert_eq!(diffs[1].filepath, "dir/b.txt");
        assert_eq!((diffs[1].new_lines, diffs[1].deleted_lines), (1, 0));
        assert_eq!(
            parse_file_data_from_diff("abc\nonly preamble\n"),
            Ok(vec![])
        );
        assert!(parse_file_data_from_diff("fatal: x").is_err());
    }

    #[test]
    fn diff_paths_use_the_last_b_separator() {
        assert_eq!(
            extract_file_path("diff --git a/my file.txt b/my file.txt"),
            Some("my file.txt")
        );
        assert_eq!(extract_file_path("diff --git a/x b/y b/z"), Some("z"));
        assert_eq!(extract_file_path("diff --git a/x b/"), None);
        assert_eq!(extract_file_path("diff --git x y"), None);
        let diffs = parse_file_data_from_diff("diff --git x y\n+a\n").unwrap();
        assert_eq!(diffs[0].filepath, "<unknown>");
    }

    #[test]
    fn pairing_drops_records_without_diffs_and_last_diff_wins() {
        let infos = vec![
            MetaFileChanges {
                filepath: "a".into(),
                old_file_path: "a".into(),
                char: ChangeType::M,
                likeness: -1,
            },
            MetaFileChanges {
                filepath: "gone".into(),
                old_file_path: "gone".into(),
                char: ChangeType::D,
                likeness: -1,
            },
        ];
        let diffs = vec![
            MetaFileChangesDiff {
                filepath: "a".into(),
                diff: vec!["first".into()],
                new_lines: 1,
                deleted_lines: 0,
            },
            MetaFileChangesDiff {
                filepath: "a".into(),
                diff: vec!["second".into()],
                new_lines: 2,
                deleted_lines: 3,
            },
        ];
        let merged = merge_file_metadata(pair_by_file_path(&infos, &diffs));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].diff, vec!["second"]);
        assert_eq!((merged[0].new_lines, merged[0].deleted_lines), (2, 3));
    }
}
