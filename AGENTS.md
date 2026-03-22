# AGENTS.md

This file is for coding agents working in `/Users/ytm-pc/Documents/codes/rust/fluent-code`.
It reflects the current Cargo workspace, repository layout, and commands verified in this repo.

## 1. Repository overview

- This repo is a Rust Cargo workspace.
- Workspace members:
  - `.` → `fluent-code` (root binary crate)
  - `crates/fluent-code-app`
  - `crates/fluent-code-provider`
  - `crates/fluent-code-tui`
- Workspace edition: `2024`
- Shared foundations include Tokio, Ratatui, Crossterm, Tracing, Serde, and `rig-core = 0.33.0`.

## 2. Crate responsibilities

### `fluent-code`
- Thin composition root in `src/main.rs`
- Loads config, builds session store/provider/runtime, launches the TUI

Keep this crate thin. Do not move domain logic here unless the user explicitly asks.

### `fluent-code-app`
- Owns application logic and durable state
- Main areas:
  - `app/` → reducer-style state transitions
  - `runtime/` → orchestration and cancellation
  - `session/` → models and persistence
  - `tool.rs` → built-in tool execution
  - `config.rs`, `error.rs`

This crate is the source of truth for operational invariants.

### `fluent-code-provider`
- Owns provider-facing logic only
- Contains mock provider behavior and `rig-core` integration
- Converts between local provider types and upstream APIs

Do not leak provider-specific concepts into `fluent-code-tui`.

### `fluent-code-tui`
- Owns Ratatui/Crossterm rendering, event handling, terminal lifecycle, and local UI state
- May coordinate effects, but should not become the home for business rules

Keep TUI-only presentation state here, not in `fluent-code-app`.

## 3. Local instruction files

Current repo state:
- Root `AGENTS.md` exists
- No `.cursorrules`
- No `.cursor/rules/`
- No `.github/copilot-instructions.md`

Do not assume hidden Cursor or Copilot rule files exist elsewhere in the repo.

## 4. Verified commands

Run commands from the repository root unless there is a strong reason not to.

### Build / check
- `cargo build --workspace`
- `cargo build -p fluent-code`
- `cargo check --workspace`

### Run
- `cargo run -p fluent-code`

### Test
- Whole workspace:
  - `cargo test --workspace`
- Single crate:
  - `cargo test -p fluent-code-app`
  - `cargo test -p fluent-code-provider`
  - `cargo test -p fluent-code-tui`

### Run a single test
Use the package name and test name.

Verified examples:
- `cargo test -p fluent-code-app creates_and_loads_latest_session`
- `cargo test -p fluent-code-app saves_and_restores_turns`
- `cargo test -p fluent-code-app mock_provider_streams_assistant_messages`
- `cargo test -p fluent-code-app cancel_aborts_active_stream_task`
- `cargo test -p fluent-code-tui persists_partial_assistant_content_before_completion`
- `cargo test -p fluent-code-tui cancel_stops_persisted_assistant_growth`

If the test name is ambiguous, add `-- --exact`.

### Format / lint
- `cargo fmt --all`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Faster crate-local loop:
  - `cargo clippy -p fluent-code-app --all-targets -- -D warnings`
  - `cargo clippy -p fluent-code-provider --all-targets -- -D warnings`
  - `cargo clippy -p fluent-code-tui --all-targets -- -D warnings`

## 5. Test layout and expectations

- Tests are inline `#[cfg(test)]` unit tests in source files.
- There is no top-level `tests/` directory.
- There is no `benches/` directory.
- Async tests use `#[tokio::test]`.
- Sync tests use plain `#[test]`.

Test-heavy files include:
- `crates/fluent-code-app/src/app/update.rs`
- `crates/fluent-code-app/src/runtime/orchestrator.rs`
- `crates/fluent-code-app/src/session/store.rs`
- `crates/fluent-code-app/src/tool.rs`
- `crates/fluent-code-tui/src/lib.rs`
- `crates/fluent-code-tui/src/view.rs`
- `crates/fluent-code-provider/src/rig.rs`

Prefer colocated tests unless there is a strong reason to introduce integration tests.

## 6. Code style guidelines

These are repo-observed conventions, not generic Rust theory.

### Imports
- Keep imports explicit.
- Group standard library, then third-party crates, then local crate imports.
- Let `rustfmt` reorder imports if it wants to.
- Use ownership-reflecting crate paths like `fluent_code_app::...`, `fluent_code_provider::...`, and `fluent_code_tui::...`.

### Formatting
- Use `cargo fmt --all`.
- Do not hand-format against `rustfmt`.
- Before finishing, prefer `cargo fmt --all -- --check`.

### Naming
- Types/enums/traits: `PascalCase`
- Functions/modules/files/tests: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
- Prefer behavior-oriented test names such as `approve_tool_executes_and_resumes_run`.

### Types and modeling
- Prefer concrete enums for state and lifecycle modeling.
- Follow the existing reducer/message/effect structure instead of bypassing it.
- Preserve existing domain models like `Msg`, `Effect`, `Session`, `Turn`, `RunRecord`, `RunStatus`, `ProviderMessage`, and `ProviderToolCall`.
- Avoid stringly-typed state when an enum or struct fits.

### Error handling
- Use crate-local `Result<T>` aliases and `thiserror` enums.
- Propagate errors with `?`.
- Use `map_err` when translating errors across subsystem boundaries.
- Avoid panics in production logic.
- `expect(...)` is acceptable in tests and tight internal invariants, but keep messages specific.

### Control flow and boundaries
- Prefer early returns for invalid, stale, or no-op paths.
- Use `match` / `if let` to keep transitions explicit.
- Write code that stays clippy-clean without suppressing lints.
- Preserve the current split:
  - app reducer mutates state
  - runtime performs async work
  - provider streams model output
  - TUI drains messages and renders
- Do not call providers directly from the reducer.
- Do not bypass runtime task tracking or cancellation.

### Persistence and invariants
- Session data is file-backed and snapshot-based.
- Persistence lives in `fluent-code-app::session::store`.
- Checkpointing is intentionally throttled.
- Keep session metadata, run outcomes, tool invocation state, and replay semantics consistent.

## 7. Architecture-specific guidance

- Keep `src/main.rs` thin.
- Keep `fluent-code-provider` independent from app-owned business rules.
- Keep `fluent-code-app` as the home of operational invariants.
- Keep `fluent-code-tui` focused on presentation and local UI state.
- When touching cross-crate APIs, update all dependent crates in the same pass.

## 8. Current behavior to preserve

- Streaming assistant output arrives incrementally.
- Cancellation is enforced by both task abort and stale-message gating.
- Checkpoint persistence is throttled, not save-on-every-chunk.
- A multi-tool assistant turn should only resume after the full batch is terminal.
- Missing-file `read` failures are recoverable tool results, not immediate run killers.
- OpenAI provider integration currently relies on latest `rig-core` with the local provider adapter.
- The TUI currently has a structured shell, compact/expanded tool detail modes, and local help/detail overlay state.

## 9. Suggested validation sequence

For single-crate changes:
1. `cargo fmt --all`
2. `cargo test -p <affected-crate>`
3. `cargo clippy -p <affected-crate> --all-targets -- -D warnings`

For broader or cross-crate work:
1. `cargo fmt --all -- --check`
2. `cargo check --workspace`
3. `cargo test --workspace`
4. `cargo clippy --workspace --all-targets -- -D warnings`

## 10. Practical guidance for coding agents

- Read crate boundaries before editing imports or moving logic.
- Prefer small, behavior-preserving changes unless the user asks for a redesign.
- Add tests when touching reducer logic, replay behavior, tool execution, provider mapping, or TUI event handling.
- If you introduce a new invariant, put it in `fluent-code-app`, not the root crate.
- If you add TUI-only state, keep it in `fluent-code-tui` and do not persist it unless explicitly required.
- Keep this file current when workspace structure, commands, or major interaction patterns change.
