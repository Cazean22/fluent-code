# AGENTS.md

Guidance for coding agents working in `/Users/ytm-pc/Documents/codes/rust/fluent-code`.

This repository is a Rust Cargo workspace. Follow the actual crate boundaries,
verified commands, and repo-local conventions below rather than generic Rust habits.

## 1. Repository overview

- Workspace members:
  - `.` → `fluent-code` (root binary crate)
  - `crates/fluent-code-app`
  - `crates/fluent-code-provider`
  - `crates/fluent-code-tui`
- Workspace edition: `2024`
- Shared foundations include Tokio, Ratatui, Crossterm, Tracing, Serde,
  `rig-core`, `pulldown-cmark`, and `syntect`.

## 2. Crate responsibilities

### `fluent-code`
- Thin composition root in `src/main.rs`
- Loads config, session store, provider client, runtime, and launches the TUI
- Keep this crate thin; do not move domain logic here unless explicitly asked.

### `fluent-code-app`
- Owns application logic and durable state
- Main areas: `app/`, `runtime/`, `session/`, `tool.rs`, `config.rs`, `error.rs`, `logging.rs`
- Source of truth for operational invariants.

### `fluent-code-provider`
- Owns provider-facing logic only
- Contains the mock provider and OpenAI/`rig-core` integration
- Converts between local provider types and upstream APIs
- Do not leak provider-specific behavior into `fluent-code-tui`.

### `fluent-code-tui`
- Owns Ratatui/Crossterm rendering, terminal lifecycle, input handling, and local UI state
- Presentation-only logic belongs here, including markdown rendering and transcript behavior
- Do not move app invariants or persistence rules into this crate.

## 3. Local instruction files

Current repo state:
- Root `AGENTS.md` exists
- No `.cursorrules`
- No `.cursor/rules/`
- No `.github/copilot-instructions.md`

Do not assume hidden Cursor or Copilot rule files exist elsewhere in this repo.

## 4. Verified commands

Run commands from the repository root unless there is a strong reason not to.

### Build / check
- `cargo build --workspace`
- `cargo build -p fluent-code`
- `cargo check --workspace`

### Run
- `cargo run -p fluent-code`

By default, the app uses the `mock` provider when no config overrides it.

### Test
- `cargo test --workspace`
- `cargo test -p fluent-code-app`
- `cargo test -p fluent-code-provider`
- `cargo test -p fluent-code-tui`

### Run a single test
Use the package plus the test name. If the test name is ambiguous, add `-- --exact`.

Verified examples:
- `cargo test -p fluent-code-app creates_and_loads_latest_session`
- `cargo test -p fluent-code-app saves_and_restores_turns`
- `cargo test -p fluent-code-app mock_provider_streams_assistant_messages`
- `cargo test -p fluent-code-app cancel_aborts_active_stream_task`
- `cargo test -p fluent-code-tui tests::persists_partial_assistant_content_before_completion -- --exact`
- `cargo test -p fluent-code-tui tests::approve_tool_executes_and_resumes_run -- --exact`

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
- Async tests use `#[tokio::test]`; sync tests use plain `#[test]`.
- Prefer colocated tests unless there is a strong reason to introduce integration tests.

Test-heavy files include:
- `crates/fluent-code-app/src/app/update.rs`
- `crates/fluent-code-app/src/runtime/orchestrator.rs`
- `crates/fluent-code-app/src/session/store.rs`
- `crates/fluent-code-app/src/tool.rs`
- `crates/fluent-code-tui/src/lib.rs`
- `crates/fluent-code-tui/src/view.rs`
- `crates/fluent-code-tui/src/conversation.rs`
- `crates/fluent-code-tui/src/markdown_render.rs`
- `crates/fluent-code-provider/src/rig.rs`

## 6. Code style guidelines

### Imports and formatting
- Keep imports explicit.
- Group standard library, then third-party crates, then local crate imports.
- Let `rustfmt` reorder imports if it wants to.
- Use `cargo fmt --all`; do not hand-format against `rustfmt`.

### Naming
- Types / enums / traits: `PascalCase`
- Functions / modules / files / tests: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
- Prefer behavior-oriented test names.
- Use descriptive variable names.

### Types and modeling
- Prefer concrete enums for state and lifecycle modeling.
- Follow the existing reducer/message/effect structure.
- Preserve domain models like `Msg`, `Effect`, `Session`, `Turn`, `RunRecord`, `RunStatus`, `ProviderMessage`, and `ProviderToolCall`.
- Avoid stringly-typed state when an enum or struct fits.

### Error handling
- Use crate-local `Result<T>` aliases and `thiserror` enums.
- Propagate errors with `?`.
- Use `map_err` when translating errors across subsystem boundaries.
- Avoid panics in production logic.
- `expect(...)` is acceptable in tests and tight invariants, but keep messages specific.

### Control flow and boundaries
- Prefer early returns for invalid, stale, or no-op paths.
- Use `match` / `if let` to keep transitions explicit.
- Keep the code clippy-clean without suppressing lints.
- Preserve the split: app reducer mutates state, runtime performs async work, provider streams model output, TUI drains messages and renders.
- Do not call providers directly from the reducer.
- Do not bypass runtime task tracking or cancellation.

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
- Markdown turn rendering is parser-driven via `pulldown-cmark`.
- Streaming markdown commits only stable lines for the active assistant turn.
- Committed fenced code blocks are syntax-highlighted; incomplete or unsupported fences fall back to plain indented code.
- The TUI has compact/expanded detail modes, local help/detail overlay state, and local transcript scroll state.

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
- Add tests when touching reducer logic, replay behavior, tool execution, provider mapping, markdown rendering, or TUI event handling.
- If you introduce a new invariant, put it in `fluent-code-app`, not the root crate.
- If you add TUI-only state, keep it in `fluent-code-tui` and do not persist it unless explicitly required.
- Keep this file current when workspace structure, commands, or major interaction patterns change.
