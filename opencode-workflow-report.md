# OpenCode Workflow Analysis

This report summarizes how the public `anomalyco/opencode` project works based on its official documentation and public repository code paths.

## Executive summary

OpenCode is not just a terminal chat app. It is a **session-centric, client/server agent runtime** where the TUI is one client, the server owns the real workflow, and prompt execution is an iterative loop that persists **sessions, messages, parts, permissions, snapshots, and summaries** while streaming structured updates to clients.

The most accurate mental model is:

**client input → server route → session prompt loop → model/tool execution → part-by-part persistence → event streaming → session completion**

A user prompt does **not** go straight from text to final answer. Instead, OpenCode:

- binds the request to a project/workspace instance,
- loads merged config and instruction files,
- persists the user message into a session,
- resolves agent/model/tools/permissions,
- runs an iterative assistant loop,
- executes tool calls as structured message parts,
- streams updates live to clients,
- and leaves behind durable session state that supports undo, share, fork, summarize, revert, and resume.

## 1. Architecture shape

### Docs-backed architecture

The official docs explicitly say:

- when you run `opencode`, it starts **"a TUI and a server"**
- and **"the TUI is the client that talks to the server"**

That is the most important architectural fact.

### Code-backed architecture

The public repo supports that statement:

- CLI execution path goes through:
  - `packages/opencode/src/cli/cmd/run.ts`
  - `packages/opencode/src/cli/bootstrap.ts`
- server surface lives in:
  - `packages/opencode/src/server/server.ts`
  - `packages/opencode/src/server/routes/session.ts`
- core runtime/orchestration lives in:
  - `packages/opencode/src/session/prompt.ts`
  - `packages/opencode/src/session/llm.ts`
  - `packages/opencode/src/session/processor.ts`
- provider/model integration lives in:
  - `packages/opencode/src/provider/provider.ts`
- tool registry lives in:
  - `packages/opencode/src/tool/*`

So OpenCode is best understood as a **backend runtime with multiple frontends**, not as "the TUI itself".

## 2. Core persisted domain model

One important thing the repo clarified: OpenCode’s primary durable model is **not** “run records” in the way our Rust rewrite currently uses them.

The public OpenCode model is mainly:

- **Session**
- **Message**
- **Part**
- **Todo**
- **Permission**
- plus diff/share/revert/summary metadata

The strongest evidence is in:

- `packages/opencode/src/session/index.ts`
- `packages/opencode/src/session/session.sql.ts`
- `packages/opencode/src/session/message-v2.ts`

### What those objects mean

- **Session** = the top-level conversation/workflow container
- **Message** = a user or assistant turn
- **Part** = a structured sub-item inside a message, like:
  - text
  - reasoning
  - tool call
  - file
  - agent mention
  - subtask
  - compaction marker
- **Permission** = pending or stored allow/ask/deny decisions
- **Todo** = tracked task list for the session

So a prompt execution in OpenCode is better thought of as:

**session + user message + assistant message + streamed parts + session status**

rather than a separate first-class persisted “run” object.

## 3. Config and bootstrap workflow

### 3.1 Bootstrap

Local CLI execution wraps work in a project instance:

- `packages/opencode/src/cli/bootstrap.ts`

It uses `Instance.provide(...)` with `InstanceBootstrap`, which strongly suggests the runtime is bound to a specific working directory/project context.

The server does something similar per request, also binding work to a workspace/project instance.

### 3.2 Config layering

OpenCode’s docs say config files are **"merged together, not replaced"**. The code in `packages/opencode/src/config/config.ts` supports that.

The documented precedence order is:

1. remote config (`.well-known/opencode`)
2. global config
3. custom config path
4. project config
5. `.opencode` directories
6. inline config

Config also loads project assets from directories such as:

- `agents/`
- `commands/`
- `modes/`
- `plugins/`
- `skills/`
- `tools/`
- `themes/`

### Workflow implication

Prompt behavior is shaped by more than a single file. By the time a prompt reaches the model, OpenCode may have merged:

- model/provider config
- agent definitions
- rules/instructions
- permissions
- commands
- plugins
- MCP servers

That makes OpenCode a **configuration-driven runtime**, not just a fixed assistant.

## 4. Instruction and rules workflow

Docs and code both show that `AGENTS.md` is part of the normal workflow, not an extra convenience.

Relevant code:
- `packages/opencode/src/session/instruction.ts`

It looks for instruction sources like:

- local `AGENTS.md`
- `CLAUDE.md`
- global `~/.config/opencode/AGENTS.md`
- extra configured instruction files
- remote instruction URLs

### Workflow implication

OpenCode builds model context from:

- current session history
- agent/system prompt
- project instructions/rules
- optionally directory-specific instruction files

So “project memory” in OpenCode is file-based and composable.

## 5. End-to-end prompt lifecycle

This is the most important section.

### 5.1 User enters a prompt

Entry modes include:

- TUI
- `opencode run`
- SDK
- server HTTP route
- slash command
- shell command path

Relevant docs:

- `/docs/tui`
- `/docs/cli`
- `/docs/server`
- `/docs/sdk`

### 5.2 Client subscribes to events

The CLI path in `packages/opencode/src/cli/cmd/run.ts` subscribes to events and watches for live updates while the prompt is processed.

This already tells you the UX is event-driven rather than “wait for one final response”.

### 5.3 Server receives the prompt

Relevant route:
- `packages/opencode/src/server/routes/session.ts`

Important endpoints include:

- `POST /:sessionID/message`
- `POST /:sessionID/prompt_async`
- `POST /:sessionID/command`
- `POST /:sessionID/shell`

Notable detail:

- `prompt_async` returns quickly and lets the real progress arrive over events

### 5.4 User message is persisted first

In `packages/opencode/src/session/prompt.ts`, `SessionPrompt.prompt(...)`:

- gets the session
- cleans revert state
- creates the user message
- touches the session
- optionally updates permission rules
- either returns immediately or enters `loop(...)`

This means the user message is durable before assistant processing starts.

### 5.5 Prompt parts are normalized

`createUserMessage(...)` turns input into internal message parts, including:

- text
- files
- agent mentions
- subtasks
- MCP resources
- synthetic read context in some cases

That is a strong sign OpenCode treats user input as **structured context**, not just one text blob.

### 5.6 The real runtime is `SessionPrompt.loop(...)`

This is the workflow heart.

Inside the loop, OpenCode:

- marks the session busy
- rebuilds message history
- finds the latest user/assistant state
- checks pending subtasks or compaction work
- resolves the active model
- may process a subtask
- may process compaction
- otherwise runs a normal assistant step

That makes OpenCode an **iterative agent loop**, not a single-shot inference wrapper.

## 6. Assistant-step workflow inside the loop

When the loop reaches a normal assistant step, it roughly does this:

1. resolve the active agent
2. resolve the model via `Provider.getModel(...)`
3. build the assistant message shell
4. resolve available tools
5. assemble system prompt and instructions
6. convert history into provider/model messages
7. hand everything to the processor/LLM layer
8. stream parts back into persistence
9. continue if there are tool calls or compaction triggers
10. stop when the assistant reaches a terminal finish state

Relevant file:
- `packages/opencode/src/session/prompt.ts`

This is where OpenCode’s workflow becomes very similar to an “agent executor” runtime.

## 7. Agents and subagents

Docs-backed agent model:

- primary agents:
  - `build`
  - `plan`
- subagents:
  - `general`
  - `explore`
- hidden/system agents:
  - `compaction`
  - `title`
  - `summary`

### What matters architecturally

- Primary agents own the main conversation
- Subagents can be invoked automatically or by `@mention`
- Subagents can create **child sessions**
- Hidden agents handle maintenance work like compaction and summarization

### Code signal

`packages/opencode/src/session/prompt.ts` explicitly handles `subtask` parts and uses `TaskTool` for delegated work.

That means subagent work is not just conceptual. It is represented in the runtime as structured, persisted workflow steps.

## 8. Provider and model invocation workflow

### 8.1 Provider registry

Relevant file:
- `packages/opencode/src/provider/provider.ts`

This file shows that OpenCode supports many providers via the AI SDK ecosystem and custom provider handling.

It contains adapters and special handling for things like:

- OpenAI
- Anthropic
- Bedrock
- Vertex
- GitLab
- OpenRouter
- OpenAI-compatible backends
- local/proxy providers

### 8.2 Model resolution

Inside the prompt loop, the model is resolved from provider/model identifiers using `Provider.getModel(...)`.

The docs say model choice may come from:

1. CLI override
2. config
3. last used model
4. internal default priority

### 8.3 The actual LLM streaming layer

Relevant file:
- `packages/opencode/src/session/llm.ts`

This is the provider-facing execution layer. It clearly uses AI SDK `streamText(...)`.

It assembles:

- system prompt pieces
- agent prompt
- user/system overrides
- provider/model-specific options
- headers
- tools
- tool choice
- abort signal
- transformed messages

So this is the clearest public proof that OpenCode’s model execution path is:

**Session loop → LLM.stream(...) → AI SDK `streamText(...)`**

## 9. Tool workflow

### 9.1 Tools are central

Docs list built-in tools such as:

- bash
- read
- edit
- write
- grep
- glob
- list
- patch
- lsp
- skill
- todowrite
- todoread
- webfetch
- websearch
- question

This is not a lightweight tool story; tool use is core to the runtime.

### 9.2 Tool resolution

Inside `SessionPrompt.resolveTools(...)`, OpenCode:

- asks `ToolRegistry` for tools
- transforms schemas for the active provider/model
- wraps tool execution with plugin hooks
- applies permissions
- injects MCP tools too

So the effective toolset for a step is:

- built-in tools
- custom/plugin tools
- MCP tools
- filtered by agent and permission rules

### 9.3 Tool execution context

Tools run with session-aware context that includes:

- session ID
- message ID
- tool call ID
- abort signal
- agent identity
- message history
- metadata callback
- permission prompt helper

That means tool execution is fully embedded in the session model.

## 10. Streaming workflow

This is one of OpenCode’s most interesting implementation choices.

### 10.1 Provider stream

At the LLM layer, model output is streamed from the provider.

### 10.2 Processor converts provider stream into parts

The repo trace found that:

- `packages/opencode/src/session/processor.ts` consumes streamed events
- it turns them into structured assistant parts such as:
  - text
  - reasoning
  - tool
  - step-start
  - step-finish

So the internal stream is not treated as plain raw text only.

### 10.3 Delta vs durable updates

One especially useful detail from the code trace:

- `updatePartDelta()` emits live `message.part.delta` events
- finalized state is durably written with `updatePart()`

That means the UX model is roughly:

**live incremental deltas first, finalized persisted parts second**

### 10.4 Server-to-client streaming

OpenCode exposes event streams through server routes, and clients subscribe to them.

The CLI uses that stream.
The web app also reduces those events client-side.

So the full streaming model is:

**provider stream → processor → session part updates/deltas → SSE/events → UI**

## 11. Persistence workflow

OpenCode persists much more than transcripts.

From the code and routes, it persists at least:

- sessions
- messages
- parts
- todo state
- permissions
- diff/snapshot metadata
- share metadata
- summary metadata
- revert state

Relevant files:

- `packages/opencode/src/session/index.ts`
- `packages/opencode/src/server/routes/session.ts`

### Supported persisted workflows

The server supports operations for:

- create/list/get/delete sessions
- fork sessions
- abort sessions
- summarize sessions
- share/unshare sessions
- diff sessions
- revert/unrevert
- get child sessions
- get todo state
- fetch messages
- patch/delete parts

So OpenCode is closer to a **durable collaborative runtime** than a simple agent shell.

## 12. Undo, revert, compaction, and sharing

### Undo / redo

Docs say `/undo` and `/redo` use Git internally and can revert:

- the last user message
- subsequent responses
- file changes

That is a strong sign OpenCode treats a prompt as a reversible work unit.

### Compaction

Docs and code both show explicit compaction behavior.

If context gets too full, OpenCode can:

- create compaction work
- summarize older context
- continue with a compacted session state

### Sharing

Docs and routes show sharing is session-level, not just export. A session can be published, unshared, and carry share metadata.

## 13. What the workflow really looks like, in one compact sequence

If I compress the whole public workflow into one chain, it looks like this:

1. user starts TUI/CLI/SDK client
2. client starts or attaches to server
3. server binds request to project/workspace instance
4. config and instruction files are merged
5. session is created/resumed/forked
6. user message is persisted
7. session status becomes busy
8. agent/model/tools/permissions/system context are resolved
9. LLM stream starts
10. processor converts stream into structured parts
11. tool calls execute as persisted tool parts
12. deltas and finalized parts are emitted to clients
13. compaction/subtasks may add more loop steps
14. assistant reaches terminal finish
15. session goes idle
16. user can continue, fork, revert, summarize, share, or inspect diffs

That is the best high-level description of the OpenCode workflow.

## 14. Most important takeaways

### 1. OpenCode is fundamentally client/server

Even local TUI use is backed by a server runtime.

### 2. Sessions are the backbone

Everything hangs off session state, not one-off prompt calls.

### 3. The runtime is iterative

A prompt is handled by a loop, not a single inference request.

### 4. Streaming is structured

OpenCode streams and persists message parts, not only raw assistant text.

### 5. Tools are first-class

Tool calls are part of the message model and permission system.

### 6. Instructions/config are deeply layered

Project behavior is shaped by merged config, `AGENTS.md`, extra instruction files, agents, permissions, and plugins.

### 7. “Run” is not the core public abstraction

The public implementation centers on sessions/messages/parts/status, not a separate dominant run record.

## 15. Best files to read next

If you want the shortest high-signal reading path through the actual code, read these in order:

1. `packages/opencode/src/cli/cmd/run.ts`
2. `packages/opencode/src/cli/bootstrap.ts`
3. `packages/opencode/src/server/server.ts`
4. `packages/opencode/src/server/routes/session.ts`
5. `packages/opencode/src/session/prompt.ts`
6. `packages/opencode/src/session/llm.ts`
7. `packages/opencode/src/session/processor.ts`
8. `packages/opencode/src/session/index.ts`
9. `packages/opencode/src/session/message-v2.ts`
10. `packages/opencode/src/provider/provider.ts`

## 16. Sources used

This report is grounded in:

- official docs pages:
  - `https://opencode.ai/docs/`
  - `/config`
  - `/providers`
  - `/agents`
  - `/tools`
  - `/tui`
  - `/cli`
  - `/server`
  - `/share`
  - `/rules`
  - `/models`
  - `/permissions`
- public repo/code paths:
  - `packages/opencode/src/session/*`
  - `packages/opencode/src/provider/*`
  - `packages/opencode/src/server/routes/session.ts`
  - `packages/opencode/src/cli/cmd/run.ts`
  - `packages/opencode/src/config/config.ts`

## 17. Suggested next step

The best follow-up would be a strict **OpenCode vs `fluent-code` architecture comparison** with columns for:

- OpenCode concept
- OpenCode file/module
- current `fluent-code` equivalent
- gaps / mismatches / opportunities
