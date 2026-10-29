# Changelog for `git-api`

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## Unreleased

### Changed

- Rewrote the service in Rust. The executable name (`git-api`), listeners
  (`0.0.0.0:8081`, plus `/tmp/haskell-ipc.sock` on Unix), HTTP/WebSocket
  protocol, JSON shapes, Git commands and parsing rules are unchanged.
- Container and CI builds use Cargo (`Dockerfile`) instead of GHC/Stack. The CI
  image tag and cache scope are unchanged.
- Cleanup of cloned repositories now also succeeds for read-only Git object
  files on Windows.

### Fixed

- Repository path segments that become `.` or `..` after removing the `.git`
  suffix (for example `owner/..git`) are rejected instead of resolving to a
  parent directory.

### Removed

- The Haskell sources, Cabal/Stack configuration, and `Dockerfile.haskell`.
  Earlier revisions remain available through Git history.

## 0.1.0 - YYYY-MM-DD
