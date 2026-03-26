# AGENTS.md

Guidance for coding agents working in `/Users/yangtingmei/Documents/codes/rust/fluent-code`.

Prefer repo-local evidence over generic Rust habits.

## Instruction sources

- This root `AGENTS.md` is the only repo-local agent instruction file.
- No `.cursorrules` file exists.
- No `.cursor/rules/` directory exists.
- No `.github/copilot-instructions.md` file exists.
- Do not assume hidden Cursor or Copilot rules exist.

## Workspace

- Workspace members:
  - `fluent-code`
  - `crates/fluent-code-app`
  - `crates/fluent-code-provider`
  - `crates/fluent-code-tui`
- Rust edition: `2024`
- `examples/plugins/echo/guest` is a standalone example guest crate, not a workspace member.

## Crate roles

- `fluent-code`: thin composition root in `src/main.rs`
- `fluent-code-app`: durable state, reducer/business logic, runtime orchestration, sessions, tools, plugins, config, errors
- `fluent-code-provider`: provider-facing logic, mock provider, Rig/OpenAI integration, provider event validation
- `fluent-code-tui`: Ratatui/Crossterm rendering, terminal lifecycle, input handling, local UI state, effect application

## Hard boundaries

- Keep `src/main.rs` thin.
- Keep durable session semantics, replay logic, permissions, and business rules in `fluent-code-app`.
- Keep provider streaming and provider validation in `fluent-code-provider`.
- Keep rendering, input mapping, wording, and local-only UI state in `fluent-code-tui`.
- Do not put async side effects directly inside the reducer.
- Do not move persisted state concerns into the TUI.

## Where to look first

- Reducer and app state:
  - `crates/fluent-code-app/src/app/message.rs`
  - `crates/fluent-code-app/src/app/state.rs`
  - `crates/fluent-code-app/src/app/update.rs`
  - `crates/fluent-code-app/src/app/permissions.rs`
- Runtime/session/tools/plugins:
  - `crates/fluent-code-app/src/runtime/orchestrator.rs`
  - `crates/fluent-code-app/src/session/model.rs`
  - `crates/fluent-code-app/src/session/store.rs`
  - `crates/fluent-code-app/src/tool.rs`
  - `crates/fluent-code-app/src/plugin/`
- Provider:
  - `crates/fluent-code-provider/src/provider.rs`
  - `crates/fluent-code-provider/src/rig.rs`
- TUI:
  - `crates/fluent-code-tui/src/lib.rs`
  - `crates/fluent-code-tui/src/events.rs`
  - `crates/fluent-code-tui/src/conversation.rs`
  - `crates/fluent-code-tui/src/view.rs`
  - `crates/fluent-code-tui/src/ui_state.rs`

## Commands

Run from the repo root unless you need package scope.

### Build / check
- `cargo check --workspace`
- `cargo build --workspace`
- `cargo check -p fluent-code-app`
- `cargo check -p fluent-code-provider`
- `cargo check -p fluent-code-tui`

### Run
- `cargo run -p fluent-code`

### Format / lint
- `cargo fmt --all`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo clippy -p fluent-code-app --all-targets -- -D warnings`
- `cargo clippy -p fluent-code-provider --all-targets -- -D warnings`
- `cargo clippy -p fluent-code-tui --all-targets -- -D warnings`

### Test
- `cargo test --workspace`
- `cargo test -p fluent-code-app`
- `cargo test -p fluent-code-provider`
- `cargo test -p fluent-code-tui`

### Single test
Use package plus a test-name substring. Add `-- --exact` when the name is module-qualified or ambiguous.

Examples:
- `cargo test -p fluent-code-app creates_and_loads_latest_session`
- `cargo test -p fluent-code-app plugin_tool_call_records_plugin_tool_source`
- `cargo test -p fluent-code-tui tests::approve_tool_executes_and_resumes_run -- --exact`
- `cargo test -p fluent-code-provider validate_openai_tool_call_id_rejects_empty_id`

### Example plugin
- `bash ./examples/plugins/echo/build.sh`

## Validation scope

### After single-crate changes
1. `cargo fmt --all`
2. `cargo test -p <affected-crate>`
3. `cargo clippy -p <affected-crate> --all-targets -- -D warnings`

### After cross-crate or shared-type changes
1. `cargo fmt --all -- --check`
2. `cargo check --workspace`
3. `cargo test --workspace`
4. `cargo clippy --workspace --all-targets -- -D warnings`

## Tests

- Tests are mostly inline `#[cfg(test)]` modules inside source files.
- There is no top-level workspace `tests/` tree.
- Sync tests use `#[test]`; async tests use `#[tokio::test]`.
- Prefer colocated tests unless the user explicitly asks for a different structure.
- Keep test names descriptive and behavior-oriented.

Common test-heavy files:
- `crates/fluent-code-app/src/app/update.rs`
- `crates/fluent-code-app/src/runtime/orchestrator.rs`
- `crates/fluent-code-app/src/session/store.rs`
- `crates/fluent-code-app/src/tool.rs`
- `crates/fluent-code-provider/src/rig.rs`
- `crates/fluent-code-tui/src/lib.rs`
- `crates/fluent-code-tui/src/view.rs`
- `crates/fluent-code-tui/src/conversation.rs`

## Code style

- Keep imports explicit and let `rustfmt` handle ordering/wrapping.
- Group imports as standard library, third-party crates, then local crate imports.
- Prefer enums and structs over stringly-typed state.
- Follow the reducer/message/effect model used in `fluent-code-app`.
- Keep control flow explicit with `match`, `if let`, and early returns.
- Use crate-local `Result<T>` aliases and `thiserror`-based errors.
- Propagate with `?`; translate errors at subsystem boundaries with `map_err` or `From`.
- Avoid panics in production logic; `expect(...)` is fine in tests and tight invariants.
- Keep code clippy-clean without suppressing lints.
- Naming: `PascalCase` for types/traits, `snake_case` for functions/modules/tests, `SCREAMING_SNAKE_CASE` for constants.

## Behavior to preserve

- Assistant output streams incrementally.
- Runtime cancellation uses task abort plus stale-message gating.
- Session checkpointing is throttled, not save-on-every-chunk.
- Multi-tool batches resume only when the full batch is terminal.
- Missing-file `read` failures are recoverable tool results, not immediate run killers.
- Task delegation uses real child-run lineage and resumes the parent with a synthetic result.
- Plugin load metadata and warnings are captured at startup and surfaced in the TUI.
- Plugin-backed tool calls record provenance in `ToolSource::Plugin`.
- TUI preserves compact vs expanded tool detail behavior and explicit transcript scroll/follow-tail state.

## Permission system

- Permission policy is app-owned, not TUI-owned.
- Tool policy is evaluated centrally in `crates/fluent-code-app/src/app/permissions.rs`.
- Built-ins and plugins both resolve through explicit tool policy in `crates/fluent-code-app/src/plugin/registry.rs`.
- Remembered approvals are session-scoped in persisted session state.
- The TUI captures replies like once / always / deny; it should not own permission rules.
- Keep deny resumable as a tool result so batch semantics remain intact.

## Plugin / provider / TUI specifics

- Plugin discovery scans configured global/project roots for subdirectories containing `plugin.toml`.
- Invalid plugins should degrade to warnings, not crash startup.
- Built-in tool names are reserved; plugins must not reuse them.
- Project plugins may override same-named global plugins.
- Current plugin capability model is intentionally strict: no filesystem, no network, no process, empty environment.
- `crates/fluent-code-provider/src/rig.rs` validates provider-emitted tool call IDs before emitting them downstream.
- UI wording changes often require updating related TUI tests in the same pass.
- Current TUI layout uses a status bar and full-width transcript; do not assume an older sidebar-based layout.

## Practical guidance

- Read local patterns before introducing new ones.
- When touching reducer logic, runtime behavior, plugin loading, provider event handling, markdown rendering, or TUI interaction, add or update tests.
- Prefer the smallest change that preserves documented invariants.
- If a change affects crate boundaries or shared types, validate the full workspace.
- Do not introduce new rule files unless the user asks.
- Update this file when workspace members, commands, rule files, or major subsystem behavior change.
