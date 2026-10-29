# Rust migration plan

Revision: 2026-10-29. Source baseline: `943e626a708a67f951e238825188be3945b291c2`.

Replace the Haskell service with a Rust implementation while preserving its observed API, Git commands, parsing rules, and request lifecycle. Source code takes precedence over contradictory README examples. The only planned behavioral correction is rejection of canonical repository path segments that become `.` or `..` after removing `.git`.

## Goals and acceptance

1. Rust is the only production implementation: `cargo build --locked --release` succeeds, the container starts `git-api`, and production build files no longer require GHC/Stack.
2. HTTP and WebSocket contracts match the source: transport integration tests cover routing, authorization precedence, request size, schema errors, envelopes, progress, and terminal closure.
3. Repository metrics match the source: deterministic local Git fixtures verify parsed JSON, ordering, initial/merge commits, file changes, contributor matching, and failure cases. Run differential checks against Haskell wherever its toolchain permits.
4. Security and lifecycle guarantees remain intact: tests cover every existing SecuritySpec and ThreadingSpec guarantee, token-scoped isolation, concurrent result sharing, retries, subprocess cleanup, and the canonical-path correction.
5. Quality and delivery gates pass: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, release build, relevant container/CI checks, and independent source-parity review. Unavailable platform/container checks must be reported as unverified.

## Scope and decisions

- Preserve executable name `git-api`, TCP listener `0.0.0.0:8081`, and Unix listener `/tmp/haskell-ipc.sock` on Unix. Keep the repository directory and remote name unchanged.
- Use one Rust crate with Tokio, explicit HTTP/WebSocket dispatch, Serde DTOs, and small modules corresponding to the source. Verify dependency APIs and commit Cargo.lock.
- Retain Git CLI argument vectors rather than introducing libgit2; the CLI options and output parsers are part of the compatibility contract.
- Use signed numeric DTO fields and explicit wire names. Domain data is owned; shared in-flight results use reference-counted values and synchronization only where needed.
- No new API, persistent metric cache, authentication policy, CORS policy, metrics algorithm, or performance claim.
- Keep Haskell available during migration for comparisons. Remove superseded production sources/build files only after Rust integration and review; Git history retains the baseline for rollback.
- One active repository writer at a time. API and core implementations are sequential; read-only analysis/review and baseline builds may run concurrently. No task worktrees are needed.
- User authorized overriding the custom workflow's 95% agent usage gate for this task.

## Compatibility inventory

### Transport and configuration

- `src/Api.hs`: GET health bypasses auth; every other request authorizes before method dispatch; POST accepts every path and does not require a JSON Content-Type. Unauthorized is 401; authorized non-POST is 405 plain text. Exactly 8192 body bytes are accepted; oversized or invalid JSON is 400. Service validation/execution failures are 500 with `err: ` prefix.
- Preserve required string `url`, optional/null string `auth_token`, ignored extra fields, URL trimming, and token-validation precedence. Characterize duplicate JSON keys and framework-owned path/handshake details before claiming exact parity.
- WebSocket upgrade is path-independent and authenticated before acceptance. Accept a single text/binary request; emit text JSON updates, terminal value/error, then close with `Done`; malformed input closes with `Bad input`. Preserve the explicit final raw-error update before the error envelope.
- Progress queue capacity is 1000 and drops on overflow. Completion must not depend on draining progress. Only a shared request's leader owns its progress stream; completed batches are not cached.
- `src/Types.hs` emits `repoName`, not the README's `name`. Preserve camelCase fields, map objects, change letters A/M/D/R/C, and string timestamps.
- `GIT_API_TOKEN` absent allows requests outside startup's exact production guard; present empty values still require a matching header. `NODE_ENV=production` requires at least 32 legacy Char8 bytes. An empty normalized allowed-host list always fails startup. Read configuration at the same request/startup boundaries.
- Preserve default hosts, ASCII-space host trimming, URL validation/error precedence, printable ASCII token limits, and public/authenticated SHA-256 path scopes. Validate raw URI syntax before a URL library can normalize away explicit ports or dot segments.
- Explicit safety correction: after suffix stripping, reject `.` and `..` (including `..git` and `...git`) before constructing canonical URL or disk path.

### Git processing and concurrency

- `src/GitCommands.hs`: preserve bare clones, credential-helper reset, optional helper token supplied only in child environment, disabled prompts, isolated working directories, and `--end-of-options` for branch revisions. Do not put tokens into URLs, argv, directory names, or diagnostics.
- `src/Command.hs`: drain stdout/stderr concurrently, decode invalid UTF-8 lossily, distinguish exit errors and stderr warnings, and retain clone's ten-second timeout. Cleanup must own child termination and wait for completion.
- `src/Parsing.hs`: delimiter is literal `\\n|||END|||`; metadata trims leading whitespace per line. Name-status splits on whitespace; only A/M/D/R/C accepted. Pair file metadata only where a diff path exists, retaining source limitations rather than silently correcting metrics.
- Diff bodies exclude `diff --git` headers but include metadata/hunk lines. Count +/- lines except +++/--- prefixes. No `--root` or merge expansion is added to `diff-tree -p`, so initial/merge commit file arrays can be empty.
- `src/Process.hs`: branch names, unique hashes, and contributor names use sorted map-key deduplication. Display name replacement and colliding names retain source behavior. Branch hashes sort by descending timestamp text with stable ties, not actual instant ordering.
- Contributor lookup uses Git author regex and default HEAD history; preserve first-seen unique email order.
- Preserve separate parsing N / command 4N capacity limits, at least one worker, reusable success/failure results, one active analysis per credential-scoped key, failed-batch retry, and directory lifetime through completion/cancellation. Cleanup errors remain visible.

## Stage 1 Source inventory and plan

- Owner: root, API architect, core architect, baseline validator.
- Dependencies: none.
- Paths: this plan; read-only `src/*.hs`, `test/*.hs`, README, Cabal/Stack, Docker and CI configuration.
- Checks: every source module maps to a Rust module and acceptance case; baseline test results recorded without claiming unavailable checks passed.
- Status: in progress.
- Outcome: API/security inventory complete; core inventory in progress; legacy `stack test --no-install-ghc` running after standard cache-access retry.

## Stage 2 Rust foundation and API

- Owner: API implementer; exclusive repository writer.
- Dependencies: stage 1 plan recorded.
- Paths: Cargo.toml, Cargo.lock, src/lib.rs, src/main.rs, src/types.rs, src/security.rs, src/api.rs, transport/security tests, temporary process interface, .gitignore.
- Boundary: `RepositoryService::new()` and async `analyze(url: String, auth_token: Option<String>, progress: tokio::sync::mpsc::Sender<String>) -> Result<RepositoryData, String>`. Service shared using Arc. Core owns final process implementation; temporary seam must not count as completed analysis behavior.
- Checks: compile foundation; DTO/security/routing tests pass; real loopback WebSocket tests use controlled service or invalid input. No network repository dependency in tests.
- Status: pending.
- Outcome: none.

## Stage 3 Git and metrics core

- Owner: core implementer; exclusive writer after stage 2 commit.
- Dependencies: stage 2 interface and DTOs.
- Paths: src/parsing.rs, src/git_commands.rs, src/command.rs, src/threading.rs, src/process.rs, core/integration tests and fixtures; Cargo changes coordinated with root.
- Checks: port existing test guarantees; fixture repositories cover root/merge commits, branches, rename/delete/binary/Unicode cases, author regex semantics, timestamp ties, errors and cleanup. Include command argv/environment tests and deterministic concurrency barriers.
- Status: pending.
- Outcome: none.

## Stage 4 Packaging and documentation

- Owner: deployment/documentation implementer; exclusive writer after stage 3.
- Dependencies: working integrated Rust service.
- Paths: Dockerfile, .dockerignore, .github/workflows/ci.yml, README.md, CHANGELOG.md, docs/architecture.md; remove superseded Haskell sources/tests/build files.
- Checks: preserve CI Docker build identity; locked build/test/lint commands in CI; non-root container includes Git and CA certificates; health probe; README DTO/config instructions match final source. Use explicit known action revisions. Preserve license and attribution.
- Status: pending.
- Outcome: none.

## Stage 5 Independent acceptance and delivery

- Owner: critic/validator read-only, root integration and fixes through assigned writer.
- Dependencies: stages 2–4.
- Checks: all five goals; compare Rust against baseline source and runnable Haskell oracle; inspect cancellation/cleanup and secret handling; run integrated checks on final revision. Confirm Windows behavior and report Linux/Unix/container evidence separately.
- Status: pending.
- Outcome: none.
- Handoff: record every unavailable check and exact cause. Commit scoped changes and push fast-forward to origin/main under existing workflow authorization. Fold durable decisions into docs/architecture.md and remove this transient plan when all stages are complete.

## Rollback and remaining risks

Rollback uses the baseline Git revision/container, not dual production implementations. Preserve the public protocol so callers need no coordinated rollout. Runtime library differences (HTTP decoding, WebSocket handshake/fragmentation, JSON duplicates, non-ASCII API keys) require explicit characterization; avoid broad claims of byte-for-byte compatibility. Platform-specific socket and process semantics need platform-specific evidence.
