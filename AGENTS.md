# AGENTS.md

Guidance for coding agents working in `/Users/yangtingmei/Documents/codes/rust/fluent-code`.

Prefer repo-local evidence over generic Rust habits.

## Workspace

- Members: `fluent-code`, `crates/fluent-code-app`, `crates/fluent-code-provider`, `crates/fluent-code-tui`
- Edition: `2024`
- Standalone example guest crate: `examples/plugins/echo/guest` (not a workspace member)

## Crate roles

- `fluent-code`: thin composition root in `src/main.rs`
- `fluent-code-app`: durable state and business logic (`app/`, `runtime/`, `session/`, `tool.rs`, `plugin/`, `config.rs`, `error.rs`)
- `fluent-code-provider`: provider-facing logic, mock provider, `rig-core` / OpenAI integration
- `fluent-code-tui`: Ratatui/Crossterm rendering, terminal lifecycle, input handling, local UI state

Keep app invariants out of `fluent-code-tui`, provider-specific logic out of `fluent-code-app`, and `src/main.rs` thin.

## Rule files

- Root `AGENTS.md` exists
- No `.cursorrules`
- No `.cursor/rules/`
- No `.github/copilot-instructions.md`

Do not assume hidden Cursor or Copilot rule files exist.

## Commands

Run from the repo root unless a manifest path is required.

### Build / check
- `cargo build --workspace`
- `cargo build -p fluent-code`
- `cargo check --workspace`
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
Use package plus a test-name substring. Add `-- --exact` for module-qualified names or ambiguity.

Examples:
- `cargo test -p fluent-code-app creates_and_loads_latest_session`
- `cargo test -p fluent-code-app plugin_tool_call_records_plugin_tool_source`
- `cargo test -p fluent-code-tui tests::approve_tool_executes_and_resumes_run -- --exact`
- `cargo test -p fluent-code-provider validate_openai_tool_call_id_rejects_empty_id`

### Example plugin
- `bash ./examples/plugins/echo/build.sh`

## Validation order

Single-crate work:
1. `cargo fmt --all`
2. `cargo test -p <affected-crate>`
3. `cargo clippy -p <affected-crate> --all-targets -- -D warnings`

Cross-crate work:
1. `cargo fmt --all -- --check`
2. `cargo check --workspace`
3. `cargo test --workspace`
4. `cargo clippy --workspace --all-targets -- -D warnings`

## Tests

- Tests are mostly inline `#[cfg(test)]` modules inside source files
- No top-level `tests/` directory
- Async tests use `#[tokio::test]`; sync tests use `#[test]`
- Prefer colocated tests unless explicitly asked for integration-test structure

Common test-heavy files:
- `crates/fluent-code-app/src/app/update.rs`
- `crates/fluent-code-app/src/runtime/orchestrator.rs`
- `crates/fluent-code-app/src/session/store.rs`
- `crates/fluent-code-app/src/tool.rs`
- `crates/fluent-code-app/src/plugin/mod.rs`
- `crates/fluent-code-tui/src/lib.rs`
- `crates/fluent-code-tui/src/view.rs`
- `crates/fluent-code-tui/src/conversation.rs`
- `crates/fluent-code-provider/src/rig.rs`

## Code style

### Imports / formatting
- Keep imports explicit
- Group standard library, then third-party crates, then local crate imports
- Let `rustfmt` handle ordering and wrapping
- Do not hand-format against `rustfmt`

### Naming
- Types / enums / traits: `PascalCase`
- Functions / modules / files / tests: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
- Prefer descriptive behavior-oriented test names

### Types / modeling
- Prefer enums and structs over stringly-typed state
- Follow the reducer/message/effect model used in `fluent-code-app`
- Preserve core types like `Msg`, `Effect`, `Session`, `Turn`, `RunStatus`, `ProviderMessage`, and `ProviderToolCall`
- Encode new invariants in types or explicit transitions when possible

### Error handling / control flow
- Use crate-local `Result<T>` aliases and `thiserror` enums
- Propagate with `?`
- Translate errors at subsystem boundaries with `map_err` or `From`
- Avoid panics in production logic
- `expect(...)` is fine in tests or tight invariants if the message is specific
- Prefer early returns for invalid, stale, or no-op paths
- Keep transitions explicit with `match`, `if let`, and narrow helpers
- Keep code clippy-clean without suppressing lints

## Architecture rules

- Keep async work in `runtime/`, not inside the reducer
- Keep provider streaming logic in `fluent-code-provider`
- Keep presentation and local-only UI state in `fluent-code-tui`
- Keep durable state, approval logic, replay logic, and session semantics in `fluent-code-app`
- When changing cross-crate APIs, update all dependent crates in the same pass

## Behavior to preserve

- Assistant output streams incrementally
- Runtime cancellation uses task abort plus stale-message gating
- Session checkpointing is throttled, not save-on-every-chunk
- Multi-tool batches resume only when the full batch is terminal
- Missing-file `read` failures are recoverable tool results, not immediate run killers
- TUI keeps compact/expanded detail modes and explicit transcript scroll/follow-tail state
- Plugin load metadata and warnings are captured at startup and surfaced in the TUI
- Plugin-backed tool calls record provenance in `ToolSource::Plugin`

## Plugin / TUI / provider specifics

- Plugin subsystem: `crates/fluent-code-app/src/plugin/`
- Startup loads plugins in `src/main.rs` via `load_tool_registry(&config)`
- Discovery scans project/global plugin roots for subdirectories containing `plugin.toml`
- Invalid plugins are disabled with warnings instead of crashing startup
- Current host only accepts zero-capability plugins: `filesystem = "none"`, `network = false`, `process = false`, `environment = []`
- Sidebar overview and operations panel are the home for operational metadata
- Compact vs expanded UI behavior is a real product pattern; preserve it
- Inline tool/provenance rendering belongs in conversation/view helpers, not app state
- If UI wording changes, update related view tests in the same pass
- In `crates/fluent-code-provider/src/rig.rs`, validate provider-emitted tool call IDs before passing events downstream

## Practical guidance

- Read crate boundaries before moving logic or changing imports
- Prefer small, behavior-preserving changes unless the user asked for redesign
- Add tests when touching reducer logic, runtime behavior, plugin loading, markdown rendering, or TUI interaction
- Do not bypass runtime task tracking or cancellation machinery
- Do not put UI-only state into persisted session data unless explicitly required
- Do not introduce new rule files unless the user asks
- Keep this file current when commands, workspace structure, or major subsystem behavior changes
