//! Wire DTOs. Field names are the camelCase names emitted by the service.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryData {
    pub repo_name: String,
    pub branches: Vec<BranchData>,
    pub all_commits: BTreeMap<String, CommitData>,
    pub contributors: BTreeMap<String, ContributorData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchData {
    pub branch_name: String,
    pub commit_hashes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitData {
    pub commit_hash: String,
    pub commit_title: String,
    pub contributor_name: String,
    pub description: String,
    pub timestamp: String,
    pub file_data: Vec<FileChanges>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContributorData {
    pub name: String,
    pub emails: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChanges {
    pub filepath: String,
    pub old_file_path: String,
    pub char: ChangeType,
    pub likeness: i64,
    pub new_lines: i64,
    pub deleted_lines: i64,
    pub diff: Vec<String>,
}

/// A = Added, M = Modified, D = Deleted, R = Renamed, C = Copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ChangeType {
    A,
    M,
    D,
    R,
    C,
}

impl ChangeType {
    pub fn from_char(value: char) -> Option<Self> {
        match value {
            'A' => Some(Self::A),
            'M' => Some(Self::M),
            'D' => Some(Self::D),
            'R' => Some(Self::R),
            'C' => Some(Self::C),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_source_field_names() {
        let change = FileChanges {
            filepath: "a".into(),
            old_file_path: "b".into(),
            char: ChangeType::R,
            likeness: 90,
            new_lines: 1,
            deleted_lines: 2,
            diff: vec![],
        };
        let value = serde_json::to_value(&change).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "filepath": "a", "oldFilePath": "b", "char": "R", "likeness": 90,
                "newLines": 1, "deletedLines": 2, "diff": []
            })
        );
        let repo = RepositoryData {
            repo_name: "r".into(),
            branches: vec![],
            all_commits: BTreeMap::new(),
            contributors: BTreeMap::new(),
        };
        let value = serde_json::to_value(&repo).unwrap();
        assert!(value.get("repoName").is_some() && value.get("allCommits").is_some());
        assert_eq!(ChangeType::from_char('X'), None);
    }
}
