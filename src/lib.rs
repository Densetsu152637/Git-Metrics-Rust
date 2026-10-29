//! HTTP and WebSocket API that clones Git repositories and reports commit,
//! branch, contributor, and file-change metrics.

pub mod api;
pub mod command;
pub mod git_commands;
pub mod parsing;
pub mod process;
pub mod security;
pub mod text;
pub mod threading;
pub mod types;
