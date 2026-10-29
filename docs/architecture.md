# Architecture

`git-api` clones a Git repository (metadata only) and reports commit, branch,
contributor, and file-change metrics over HTTP and WebSocket. It is a single
Rust crate (library `git_api`, executable `git-api`) built on Tokio and axum.
The Haskell implementation it replaced remains in Git history at
`943e626a708a67f951e238825188be3945b291c2`; roll back by deploying that revision
or its container. The wire protocol is unchanged, so callers need no rollout.

## Request lifecycle

1. `api` dispatches every request. A WebSocket upgrade (`Upgrade: websocket`
   and a `Connection: upgrade` token) goes to the WebSocket flow; anything else
   to the HTTP flow. Neither depends on the path.
2. Authorization (`X-Git-Api-Key` against `GIT_API_TOKEN`, read per request)
   runs before method dispatch. `GET /health` is the only exception.
3. The payload (`url`, optional `auth_token`; at most 8192 bytes) goes to
   `RepositoryService::analyze`, which validates the token, then the URL (token
   errors win), derives a credential-scoped key, and shares one analysis per key.
4. The leader clones into `<tmp>/cloned-repos/<scope>/<host>/<owner>/<repo>`,
   runs the Git commands in worker pools, assembles `RepositoryData`, and removes
   the clone when its last user releases it.

Progress is a bounded channel (capacity 1000) filled with `try_send`: updates are
dropped when it is full or closed, so completion never depends on a client
draining it. Only the leader of a shared analysis owns a progress stream.

## Modules

| Module | Responsibility |
| --- | --- |
| `api` | HTTP and WebSocket transport, envelopes, size limits, authorization order. Generic over an `Analyzer` so tests use a controlled service. |
| `security` | URL validation on the raw RFC 3986 syntax, token validation, SHA-256 credential scopes, constant-time key comparison, startup checks. |
| `process` | `RepositoryService`: validation order, clone lifecycle, per-directory locks and reference counts, metric assembly. |
| `threading` | Request batching (`AwaitBatch`), bounded worker pools, two-stage pipelines, progress `emit`. |
| `command` | Child process execution (concurrent stdout/stderr drain, lossy UTF-8, timeout, kill on drop) and directory deletion with retries. |
| `git_commands` | Git argument vectors and child environments. Nothing is built through a shell. |
| `parsing` | Parsers for Git output; they keep the source's documented limitations. |
| `text` | Haskell-compatible `lines`, `words`, `isSpace`, `show`, `nub`. |
| `types` | Serde DTOs with the wire field names. |

## Decisions

- **Git CLI, not libgit2.** The option set and the output parsers are part of the
  compatibility contract.
- **Credentials stay out of URLs, argv, paths and diagnostics.** A token reaches
  Git only through the child environment (`GIT_AUTH_TOKEN`) and a one-shot
  credential helper. Authenticated clones live under `authenticated-<sha256>`;
  public ones under `public`. A request without the token can never join or reuse
  an authenticated analysis.
- **One analysis per credential-scoped key**, shared with concurrent callers via
  reference-counted results. Completed batches are not cached; failed batches are
  retried by the next request. If a leader is cancelled, its followers receive an
  error instead of a result.
- **Pools bound concurrency**: `N` parsing workers and `4N` command workers for
  `N` logical cores, never fewer than one. Parsing runs on blocking threads.
- **Cancellation is structural.** Dropping an analysis aborts its queued tasks,
  kills its child processes (Tokio `kill_on_drop`), and releases its directory in
  the background. Cleanup failures stay visible as the request error.
- **Safety correction.** After removing a `.git` suffix, repository path segments
  `.` and `..` (for example `..git`, `...git`) are rejected before any URL or disk
  path is built. The Haskell service probed `owner/.` for `owner/..git`.

## Retained behaviours (by design)

- `diff-tree -p` is used without `--root` or merge expansion, so root and merge
  commits have empty `fileData`; without rename detection a renamed file's diff is
  a full addition of the new path.
- Name-status lines are split on whitespace and diff paths are taken after the
  last ` b/`, so paths containing spaces or ` b/` can be mis-parsed or dropped.
- Branch hashes sort by descending timestamp *text* with stable ties.
- Names and branch prefixes: `origin/` then `remotes/` are removed once, wherever
  they first occur; duplicate results collapse by sorted map keys.
- `repoName` is the last `/` component of the `origin` URL (not `name`).
- Error text for validation and parse failures matches the source, including the
  `user error (...)` wrapper on analysis failures.

## Known differences and characterisation limits

The two implementations are compared on identical fixtures (see Testing), not
claimed byte-for-byte identical.

- JSON object keys are emitted in sorted order. Duplicate keys: the last one wins
  in both. A JSON string with a lone surrogate escape is rejected (Haskell
  substituted U+FFFD) and nesting deeper than 128 levels is rejected.
- Unauthenticated WebSocket upgrades are rejected with `400 Unauthorized`.
  Handshake details otherwise follow axum/tungstenite (for example the upgrade
  must be a `GET`), and responses lack the Haskell server's `Date`/`Server` headers.
- Warp's connection timeouts have no counterpart: the server sets no header-read
  or idle timeout, so deploy it behind a proxy that does.
- Child processes get a null stdin. On Unix a stale `/tmp/haskell-ipc.sock` is
  not removed at startup (as before), so a restart may need it deleted.
- `GIT_API_TOKEN` is compared as legacy Char8 bytes (each character truncated to
  8 bits), as before; non-UTF-8 environment values are decoded lossily.
- On Windows the Haskell service could not delete Git's read-only object files
  and failed after every analysis; the Rust service deletes them.
- Remaining queued progress updates are dropped once the result is ready (the
  Haskell behaviour was equally unordered).

## Testing

- Unit tests per module cover URL/token/key rules, command argument vectors and
  environments, parsers, batching, pools, directory lifetime, and cancellation.
- `tests/analysis.rs` builds deterministic local repositories (root, merge,
  rename, delete, binary, Unicode, equal timestamps) and runs the real clone
  pipeline offline through `url.<base>.insteadOf`.
- `tests/transport.rs` runs a loopback server with a controlled analyzer for
  routing, authorization precedence, size limits, schema errors, envelopes,
  progress, and WebSocket closure.
- `tests/differential.rs` (ignored by default) starts a Haskell `git-api` binary
  (`HASKELL_GIT_API_EXE`, port 8081) and requires identical JSON for a fixture
  and identical failure bodies, except the intentional `..git` correction.
