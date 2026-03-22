# AGENTS.md

This file is for coding agents working in `/Users/yangtingmei/Documents/codes/rust/fluent-code`.
It reflects the current repository state, the actual Cargo workspace layout, and the commands verified in this repo.

## 1. Repository overview

- This repo is a Rust Cargo workspace.
- Root package: `fluent-code` (thin binary crate in `src/main.rs`).
- Workspace member crates:
  - `crates/fluent-code-app`
  - `crates/fluent-code-provider`
  - `crates/fluent-code-tui`
- The root crate wires config, session store, provider, runtime, and TUI together.
- The root crate is intentionally thin; do not move business logic into `src/main.rs` unless the user explicitly wants wiring changed.

## 2. Crate responsibilities

### `fluent-code`
- Composition root only.
- Loads config.
- Constructs the session store.
- Constructs the provider.
- Constructs the runtime.
- Launches the TUI.

### `fluent-code-app`
- Application logic and durable state.
- Owns:
  - `app/` reducer-style state transitions
  - `runtime/` orchestration and task cancellation
  - `session/` models and file-backed persistence
  - `config.rs`
  - `error.rs`
- This crate currently contains the main operational invariants of the app.

### `fluent-code-provider`
- Provider-facing logic only.
- Owns mock provider behavior and `rig-core` integration.
- Keep provider-specific request/response mapping here.
- Do not leak `rig-core` details into `fluent-code-tui`.

### `fluent-code-tui`
- Ratatui/crossterm frontend.
- Owns event handling, rendering, terminal lifecycle, and the UI loop.
- It may coordinate effect execution, but it should not become the home for core domain rules.

## 3. Local instruction files

Current repo state:
- No `.cursorrules` file exists.
- No `.cursor/rules/` directory exists.
- No `.github/copilot-instructions.md` file exists.

Do not assume hidden policy files exist elsewhere in the repo.

## 4. Verified commands

Run commands from the repository root unless there is a strong reason not to.

### Build

- Build the whole workspace:
  - `cargo build --workspace`
- Build only the root binary package:
  - `cargo build -p fluent-code`

### Run

- Run the app:
  - `cargo run -p fluent-code`

### Test

- Run all tests in the workspace:
  - `cargo test --workspace`
- Run tests for one crate:
  - `cargo test -p fluent-code-app`
  - `cargo test -p fluent-code-provider`
  - `cargo test -p fluent-code-tui`

### Run a single test

Use the package name and the exact test name.

Examples verified from current source files:
- `cargo test -p fluent-code-app creates_and_loads_latest_session`
- `cargo test -p fluent-code-app saves_and_restores_turns`
- `cargo test -p fluent-code-app mock_provider_streams_assistant_messages`
- `cargo test -p fluent-code-app cancel_aborts_active_stream_task`
- `cargo test -p fluent-code-tui persists_partial_assistant_content_before_completion`
- `cargo test -p fluent-code-tui cancel_stops_persisted_assistant_growth`

If you need exact matching for an ambiguous name, add `-- --exact`.

### Formatting

- Format the whole workspace:
  - `cargo fmt --all`
- Verify formatting without changing files:
  - `cargo fmt --all -- --check`

### Linting

- Run clippy across the whole workspace:
  - `cargo clippy --workspace --all-targets -- -D warnings`

This command is valid in the current repo and should be treated as the main lint gate.

## 5. Test layout and expectations

- Tests are inline unit tests under `#[cfg(test)]` in source files.
- There is no top-level `tests/` directory right now.
- There is no `benches/` directory right now.
- Async behavior uses `#[tokio::test]`.
- Sync file-store tests use plain `#[test]`.
- Test helpers are usually private functions at the bottom of the same module.

Existing test-heavy files:
- `crates/fluent-code-app/src/session/store.rs`
- `crates/fluent-code-app/src/runtime/orchestrator.rs`
- `crates/fluent-code-tui/src/lib.rs`

When adding tests, prefer colocated tests unless there is a strong reason to introduce integration tests.

## 6. Code style guidelines

These are repo-observed conventions, not generic Rust theory.

### Imports

- Keep imports explicit.
- Prefer grouping standard library imports first, then third-party imports, then local crate imports.
- If `rustfmt` reorders imports, follow the formatter.
- Use crate paths that reflect ownership boundaries:
  - `fluent_code_app::...`
  - `fluent_code_provider::...`
  - `fluent_code_tui::...`

### Formatting

- Use `cargo fmt --all`.
- Do not hand-format in ways that fight rustfmt.
- Before finishing a change, prefer `cargo fmt --all -- --check`.

### Naming

- Types and enums: `PascalCase`
- Functions, modules, files, and tests: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
- Prefer behavior-oriented test names like:
  - `creates_and_loads_latest_session`
  - `cancel_stops_persisted_assistant_growth`

### Types and modeling

- Prefer concrete enums for state transitions and lifecycle states.
- This repo already uses message/effect/state patterns; follow them instead of bypassing them.
- Preserve lightweight domain modeling:
  - `Msg`
  - `Effect`
  - `Session`
  - `Turn`
  - `RunRecord`
  - `RunStatus`
- Avoid introducing loosely typed stringly-typed state if an enum is more appropriate.

### Error handling

- Use crate-local `Result<T>` aliases and `thiserror` enums.
- Propagate errors with `?` in production code.
- Use `map_err` when crossing subsystem boundaries and you need to translate errors.
- Avoid panics in production logic.
- `expect(...)` is acceptable in tests and for truly internal invariants, but keep messages specific.

### Control flow

- Prefer early returns for invalid or no-op paths.
- Use `match` and `if let` to make message/state transitions explicit.
- Follow clippy-friendly patterns; this repo currently passes:
  - `cargo clippy --workspace --all-targets -- -D warnings`
- If clippy suggests collapsing nested `if` statements, do it rather than suppressing the lint.

### Concurrency and async

- Runtime work is background-driven with Tokio tasks.
- Preserve the existing split:
  - app reducer mutates state
  - runtime performs async work
  - provider streams model output
  - TUI drains messages and renders
- Do not move provider calls directly into the reducer.
- Do not bypass runtime cancellation/task tracking.

### Persistence

- Session data is file-backed and currently snapshot-based.
- Session persistence lives in `fluent-code-app::session::store`.
- Checkpointing is throttled; do not turn it into save-on-every-chunk unless intentionally redesigning that behavior.
- Keep session and run metadata consistent when changing streaming, cancellation, or persistence behavior.

## 7. Architecture-specific guidance

- Keep `src/main.rs` thin.
- Keep `fluent-code-provider` independent of app-owned business logic.
- Keep `fluent-code-tui` focused on UI concerns and effect execution, not domain modeling.
- Keep `fluent-code-app` as the home of reducer logic, session invariants, and runtime state transitions.
- Avoid creating micro-crates for `config`, `error`, `store`, or `checkpoint` unless the user explicitly wants another split.

## 8. What to preserve when editing

- Streaming updates arrive incrementally through runtime/provider boundaries.
- Cancellation is enforced both by task abort and stale-message gating.
- Checkpoint persistence is intentionally throttled.
- Run outcomes are persisted in session metadata.
- The workspace currently passes build, test, fmt, and clippy from the repo root.

If your change breaks one of those properties, either fix it or document the change clearly.

## 9. Suggested validation sequence for most code changes

For local changes in one crate:
1. `cargo fmt --all`
2. `cargo test -p <affected-crate>`
3. `cargo clippy -p <affected-crate> --all-targets -- -D warnings` if you want a faster local loop

Before finishing broader or cross-crate work:
1. `cargo fmt --all -- --check`
2. `cargo check --workspace`
3. `cargo test --workspace`
4. `cargo clippy --workspace --all-targets -- -D warnings`

## 10. Practical agent guidance

- Read the crate boundaries before editing imports.
- Prefer small, behavior-preserving changes.
- When touching cross-crate APIs, update all dependent crates in the same pass.
- If you add a new test, make it runnable with a simple `cargo test -p <crate> <name>` command.
- If you introduce a new operational invariant, place it in `fluent-code-app`, not in the root binary.

This file should be kept current as the workspace grows.
