# AGENTS.md
Guidance for coding agents working in `/Users/yangtingmei/Documents/codes/rust/fluent-code`.
Prefer repo-local evidence over generic Rust habits.

## Instruction sources
- This root `AGENTS.md` is the repo-local instruction file.
- No `.cursorrules` file exists.
- No `.cursor/rules/` directory exists.
- No `.github/copilot-instructions.md` file exists.
- Do not assume hidden Cursor or Copilot rules exist.

## Workspace
- Cargo workspace members:
  - `fluent-code` (`.`)
  - `crates/fluent-code-app`
  - `crates/fluent-code-provider`
  - `crates/fluent-code-tui`
- Rust edition: `2024`
- `examples/plugins/echo/guest` is a standalone example guest crate, not a workspace member.

## Crate roles
- `src/main.rs`: thin composition root.
- `fluent-code-app`: durable session model, reducer logic, permissions, replay, recovery, tools, plugins, config.
- `fluent-code-provider`: provider-facing request/streaming types, mock provider, Rig/OpenAI integration.
- `fluent-code-tui`: terminal lifecycle, rendering, input mapping, local UI state, effect application.

## Hard boundaries
- Keep `src/main.rs` thin.
- Keep durable state, replay rules, permissions, batching, delegation, and startup recovery in `fluent-code-app`.
- Keep provider streaming and provider validation in `fluent-code-provider`.
- Keep rendering, wording, input mapping, and local-only UI state in `fluent-code-tui`.
- Runtime is an executor, not a scheduler or persistence layer.
- Do not put async side effects directly inside the reducer.
- Do not move persisted state concerns into the TUI.

## Start here
- Reducer/state/messages: `crates/fluent-code-app/src/app/{message,state,update}.rs`
- Delegation/recovery/replay: `crates/fluent-code-app/src/app/{delegation,recovery,request_builder}.rs`
- Session/runtime/tools/plugins: `crates/fluent-code-app/src/{session,runtime,tool,plugin}/`
- Provider: `crates/fluent-code-provider/src/{provider,rig}.rs`
- TUI: `crates/fluent-code-tui/src/{lib,conversation,view,events}.rs`

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

### Run a single test
- Pattern: `cargo test -p <crate> <test_name_substring>`
- Use exact matching for module-qualified or ambiguous names:
  - `cargo test -p <crate> <module_path>::<test_name> -- --exact`
- Examples:
  - `cargo test -p fluent-code-app creates_and_loads_latest_session`
  - `cargo test -p fluent-code-app plugin_tool_call_records_plugin_tool_source`
  - `cargo test -p fluent-code-tui tests::approve_tool_executes_and_resumes_run -- --exact`
  - `cargo test -p fluent-code-provider validate_openai_tool_call_id_rejects_empty_id`

### Example plugin
- `bash ./examples/plugins/echo/build.sh`

## Validation policy
- After single-crate changes:
  1. `cargo fmt --all`
  2. `cargo test -p <affected-crate>`
  3. `cargo clippy -p <affected-crate> --all-targets -- -D warnings`
- After cross-crate or shared-type changes:
  1. `cargo fmt --all -- --check`
  2. `cargo check --workspace`
  3. `cargo test --workspace`
  4. `cargo clippy --workspace --all-targets -- -D warnings`

## Testing conventions
- Tests are mostly inline `#[cfg(test)]` modules inside source files.
- There is no top-level workspace `tests/` tree.
- Sync tests use `#[test]`; async tests use `#[tokio::test]`.
- Prefer colocated tests unless the user asks for another structure.
- Keep test names descriptive and behavior-oriented.
- When touching reducer logic, runtime behavior, provider streaming, plugin loading, markdown rendering, or TUI interaction, update tests in the same pass.
- Test-heavy files include:
  - `crates/fluent-code-app/src/app/update.rs`
  - `crates/fluent-code-app/src/app/delegation.rs`
  - `crates/fluent-code-app/src/app/recovery.rs`
  - `crates/fluent-code-app/src/runtime/orchestrator.rs`
  - `crates/fluent-code-app/src/session/store.rs`
  - `crates/fluent-code-provider/src/rig.rs`
  - `crates/fluent-code-tui/src/{lib,view,conversation}.rs`

## Code style
- Keep imports explicit; let `rustfmt` handle ordering and wrapping.
- Group imports as standard library, third-party crates, then local crate imports.
- Prefer enums and structs over stringly-typed state.
- Follow the reducer/message/effect model already used in `fluent-code-app`.
- Keep control flow explicit with `match`, `if let`, and early returns.
- Use crate-local `Result<T>` aliases and `thiserror`-based errors where a subsystem already does.
- Propagate with `?`; translate errors at subsystem boundaries with typed error enums, `From`, or `map_err`.
- Avoid panics in production logic; `expect(...)` is acceptable in tests and tight invariants.
- Keep code clippy-clean without suppressing lints.
- Naming: `PascalCase` for types/traits, `snake_case` for functions/modules/tests, `SCREAMING_SNAKE_CASE` for constants.
- Prefer small targeted helpers over clever abstraction, especially in reducer and recovery code.

## Behavior invariants to preserve
- Assistant output streams incrementally.
- Runtime cancellation uses task abort plus stale-message gating by `run_id`.
- Session checkpointing is throttled, not save-on-every-chunk.
- Multi-tool batches resume only when the full batch is terminal.
- Permission policy is app-owned; the TUI collects replies but does not own rule evaluation.
- Remembered approvals are persisted in session state.
- Missing-file `read` failures are recoverable tool results, not immediate run killers.
- Task delegation uses real child-run lineage and resumes the parent with a synthetic result.
- Startup persists a narrow foreground owner so root generation and awaiting-tool-approval can be restored safely.
- Interrupted delegated child runs may be terminalized on startup; malformed or ambiguous lineage must fail closed.
- Generic in-flight tool restart still fails closed rather than guessing.
- Child requests stay leaf-only; do not re-enable recursive task delegation accidentally.
- Plugin load metadata and warnings are captured at startup and surfaced in the TUI.
- Plugin-backed tool calls record provenance in `ToolSource::Plugin`.
- Invalid plugins should degrade to warnings, not crash startup.
- Built-in tool names are reserved; plugins must not reuse them.
- The current plugin capability model is intentionally strict.
- `crates/fluent-code-provider/src/rig.rs` validates provider-emitted tool call IDs before emitting them downstream.
- UI wording and transcript formatting changes usually require TUI test updates in the same pass.

## Practical guidance
- Read local patterns before introducing new ones.
- Prefer the smallest change that preserves documented invariants.
- If a change affects crate boundaries or shared session/provider types, validate the full workspace.
- Do not introduce new rule files unless the user asks.
- Update this file when workspace members, commands, recovery behavior, or major subsystem boundaries change.
