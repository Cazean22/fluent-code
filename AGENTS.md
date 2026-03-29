# AGENTS.md
Guidance for coding agents working in `/Users/ytm-pc/Documents/codes/rust/fluent-code`.
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
  - `crates/fluent-code-acp`
  - `crates/fluent-code-app`
  - `crates/fluent-code-provider`
  - `crates/fluent-code-tui`
- Rust edition: `2024`
- `examples/plugins/echo/guest` is a standalone example guest crate, not a workspace member.

## Crate roles
- `src/main.rs`: TUI-first thin composition root that calls the root entrypoint wrapper.
- `src/lib.rs`: root entrypoint wrappers for the default TUI path and the secondary ACP binary.
- `fluent-code-acp`: ACP server entrypoint, JSON-RPC session lifecycle, stdio/jsonl harness, contract coverage.
- `fluent-code-app`: durable session model, reducer logic, permissions, replay, recovery, tools, plugins, config, and shared startup bootstrap.
- `fluent-code-provider`: provider-facing request and streaming types, mock provider, Rig/OpenAI integration.
- `fluent-code-tui`: terminal lifecycle, rendering, input mapping, local UI state, effect application.

## Hard boundaries
- Keep `src/main.rs` thin.
- Keep root entrypoint wrappers in `src/lib.rs`; do not move bootstrap or protocol logic into the root crate.
- Keep ACP protocol handling, session new/load/prompt/cancel flows, and ACP harness behavior in `fluent-code-acp`.
- Keep durable state, replay rules, permissions, batching, delegation, and startup recovery in `fluent-code-app`.
- Keep shared config/logging/store/provider/plugin/runtime bootstrap in `fluent-code-app`, not `fluent-code-tui`.
- Keep provider streaming and provider validation in `fluent-code-provider`.
- Keep rendering, wording, input mapping, and local-only UI state in `fluent-code-tui`.
- Runtime is an executor, not a scheduler or persistence layer.
- Do not put async side effects directly inside the reducer.
- Do not move persisted state concerns into the TUI.

## Start here
- Root entrypoints: `src/main.rs`, `src/lib.rs`, `src/bin/fluent-code-acp.rs`
- ACP server and contract tests: `crates/fluent-code-acp/src/server/{mod,contract_tests}.rs`
- Shared bootstrap: `crates/fluent-code-app/src/bootstrap.rs`
- Reducer, state, messages: `crates/fluent-code-app/src/app/{message,state,update}.rs`
- Delegation, recovery, replay: `crates/fluent-code-app/src/app/{delegation,recovery,request_builder}.rs`
- App internals: `crates/fluent-code-app/src/{session,runtime,tool,plugin}/`
- Provider and TUI: `crates/fluent-code-provider/src/{provider,rig}.rs`, `crates/fluent-code-tui/src/{lib,conversation,view,events}.rs`

## Commands
Run from the repo root unless package scope is required.

### Build / check
- `cargo check --workspace`
- `cargo build --workspace`
- `cargo check -p fluent-code-acp`
- `cargo check -p fluent-code-app`
- `cargo check -p fluent-code-provider`
- `cargo check -p fluent-code-tui`

### Run
- `cargo run -p fluent-code`
- `cargo run -p fluent-code --bin fluent-code-acp`

### Format / lint
- `cargo fmt --all`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo clippy -p fluent-code-acp --all-targets -- -D warnings`
- `cargo clippy -p fluent-code-app --all-targets -- -D warnings`
- `cargo clippy -p fluent-code-provider --all-targets -- -D warnings`
- `cargo clippy -p fluent-code-tui --all-targets -- -D warnings`

### Test
- `cargo test --workspace`
- `cargo test -p fluent-code-acp`
- `cargo test -p fluent-code-app`
- `cargo test -p fluent-code-provider`
- `cargo test -p fluent-code-tui`

### Run a single test
- Pattern: `cargo test -p <crate> <test_name_substring>`
- Use exact matching for module-qualified or ambiguous names:
  - `cargo test -p <crate> <module_path>::<test_name> -- --exact`
- Current examples:
  - `cargo test -p fluent-code-acp contract_initialize_rejects_unsupported_protocol_version`
  - `cargo test -p fluent-code-acp contract_live_same_connection_cancel_resolves_prompt_over_stdio_loop`
  - `cargo test -p fluent-code-app creates_and_loads_latest_session`
  - `cargo test -p fluent-code-provider validate_openai_tool_call_id_rejects_empty_id`
  - `cargo test -p fluent-code-tui tests::approve_tool_executes_and_resumes_run -- --exact`
- Plugin example build: `bash ./examples/plugins/echo/build.sh`

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
- ACP contract coverage is in `crates/fluent-code-acp/src/server/contract_tests.rs`.
- Root-package binary wiring coverage lives in `tests/entrypoints.rs`.
- Some crate-level exact-name startup coverage lives in `crates/fluent-code-app/tests/` and `crates/fluent-code-tui/tests/` when command-level verification needs an integration-test surface.
- Sync tests use `#[test]`; async tests use `#[tokio::test]`.
- Prefer colocated tests unless the user asks for another structure.
- Keep test names descriptive and behavior-oriented.
- When touching reducer logic, ACP session flows, runtime behavior, provider streaming, plugin loading, markdown rendering, or TUI interaction, update tests in the same pass.
- Test-heavy files include:
  - `crates/fluent-code-acp/src/server/contract_tests.rs`
  - `crates/fluent-code-app/src/app/{update,delegation,recovery}.rs`
  - `crates/fluent-code-app/tests/bootstrap.rs`
  - `crates/fluent-code-app/src/{runtime/orchestrator,session/store}.rs`
  - `crates/fluent-code-provider/src/rig.rs`
  - `crates/fluent-code-tui/src/{lib,view,conversation}.rs`
  - `crates/fluent-code-tui/tests/startup.rs`

## Code style
- Keep imports explicit. Let `rustfmt` handle ordering and wrapping.
- Group imports as standard library, third-party crates, then local crate imports.
- Prefer enums and structs over stringly-typed state.
- Follow the reducer/message/effect model already used in `fluent-code-app`.
- Keep ACP request and notification handling explicit. Match protocol methods and stop reasons directly.
- Keep control flow explicit with `match`, `if let`, and early returns.
- Use crate-local `Result<T>` aliases and `thiserror`-based errors where a subsystem already does.
- Propagate with `?`. Translate errors at subsystem boundaries with typed error enums, `From`, or `map_err`.
- Avoid panics in production logic. `expect(...)` is fine in tests and tight invariants.
- Keep code clippy-clean without suppressing lints.
- Naming: `PascalCase` for types and traits, `snake_case` for functions, modules, and tests, `SCREAMING_SNAKE_CASE` for constants.
- Prefer small targeted helpers over clever abstraction, especially in reducer, recovery, and ACP contract code.

## Behavior invariants to preserve
- Assistant output streams incrementally.
- Runtime cancellation uses task abort plus stale-message gating by `run_id`.
- Session checkpointing is throttled, not save-on-every-chunk.
- Multi-tool batches resume only when the full batch is terminal.
- Permission policy is app-owned. The TUI collects replies but does not own rule evaluation.
- Remembered approvals are persisted in session state.
- Missing-file `read` failures are recoverable tool results, not immediate run killers.
- Task delegation uses real child-run lineage and resumes the parent with a synthetic result.
- Startup persists a narrow foreground owner so root generation and awaiting-tool-approval can be restored safely.
- Interrupted delegated child runs may be terminalized on startup. Malformed or ambiguous lineage must fail closed.
- Generic in-flight tool restart still fails closed rather than guessing.
- Child requests stay leaf-only. Do not re-enable recursive task delegation accidentally.
- Plugin load metadata and warnings are captured at startup and surfaced in the TUI.
- Plugin-backed tool calls record provenance in `ToolSource::Plugin`.
- Invalid plugins should degrade to warnings, not crash startup.
- Built-in tool names are reserved. Plugins must not reuse them.
- The current plugin capability model is intentionally strict.
- `crates/fluent-code-provider/src/rig.rs` validates provider-emitted tool call IDs before emitting them downstream.
- ACP initialize must stay first, protocol version checks must fail cleanly, and session prompt/load/cancel flows must keep contract coverage.
- UI wording and transcript formatting changes usually require TUI test updates in the same pass.

## Practical guidance
- Read local patterns before introducing new ones.
- Prefer the smallest change that preserves these invariants.
- If a change affects crate boundaries or shared session/provider/ACP types, validate the full workspace.
- Do not introduce new rule files unless the user asks.
- Keep the default user-facing binary TUI-first and preserve ACP as the secondary binary unless the user explicitly requests another startup topology.
- Update this file when workspace members, commands, recovery behavior, or major subsystem boundaries change.
