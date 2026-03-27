# OpenCode vs. fluent-code Architecture

This document compares the current architecture described in the grounded OpenCode summary with the current implementation in this Rust repo. It is comparison-first, not a full OpenCode workflow rewrite. For the fuller workflow narrative, see [`../opencode-workflow-report.md`](../opencode-workflow-report.md). For the current delegated scheduling implementation details, see [`opencode-delegated-scheduling.md`](opencode-delegated-scheduling.md).

`docs/agent-architecture-spec.md` is a broader target model, not a description of the current implementation in this repo.

## Purpose and non-goals

### Purpose

- map equivalent concepts between OpenCode and this repo
- show where the current Rust architecture already matches OpenCode closely
- isolate the highest-value gaps for a phase-2 port

### Non-goals

- re-explain the full OpenCode request loop from scratch
- restate the entire phase-1 delegated scheduling design
- treat `docs/agent-architecture-spec.md` as already implemented
- infer behavior beyond the grounded summaries

## High-level architecture shape

OpenCode is a session-centric client/server runtime. One server can host multiple project or workspace instances, streaming structured session events to clients while durable session state stays central.

This repo is a local, in-process composition: `src/main.rs` loads the latest session, constructs provider, runtime, and TUI, then hands control to the TUI loop. Durable semantics live in `fluent-code-app`, provider streaming lives in `fluent-code-provider`, and rendering plus local UI state live in `fluent-code-tui`.

That difference matters more than implementation language. OpenCode centers execution around a server-owned session engine. This repo centers execution around an app-owned reducer with a thin runtime executor.

## Concept mapping

| OpenCode concept | fluent-code counterpart | Current match |
|---|---|---|
| Client/server runtime, one server hosting many workspace instances | Local in-process app composed from `main.rs`, app, runtime, provider, and TUI | Partial |
| Session as primary durable unit | `Session` in `crates/fluent-code-app/src/session/model.rs` | Strong |
| First-class session lineage via parentID, forks, children | Run and delegation lineage via `RunRecord`, `TaskDelegationRecord`, `parent_run_id`, `parent_tool_invocation_id` inside one session | Partial |
| Message model as structured parts | `Turn` plus run, tool, and delegation records | Partial |
| Explicit built-in agent taxonomy with config for prompt, model, permissions, steps | `agent.rs` task tool contract and delegated-agent lookup | Partial |
| Layered pattern-based permission policy, resumable approvals | App-owned permissions in `app/permissions.rs`, tool policy defaults in `plugin/registry.rs`, resumable approval flow in reducer | Strong |
| Prompt loop as orchestration core | Reducer protocol in `app/message.rs` and `app/update.rs`, runtime only executes effects | Partial |
| Background work as session-centric async prompt execution with status observation | In-memory run task registry in `runtime/orchestrator.rs`, foreground handoff via `active_run_id` | Partial |
| Typed session status and built-in retry | Durable run and delegation status, provider startup retry before first event | Partial |
| Part-level streaming over bus or SSE | Provider stream forwarding and streaming delta handling through reducer | Partial |
| Hybrid persistence, SQLite plus file-backed artifacts | File-backed session store with `session.json`, `turns.jsonl`, `latest_session` | Partial |
| Stateful recovery, retry, abort, revert, unrevert, compaction mutate durable session state | Cancellation, throttled checkpointing, durable session records | Limited |

## Lifecycle comparison

| Stage | OpenCode | fluent-code today |
|---|---|---|
| Bootstrap | Client talks to a server bound to a workspace instance | `main.rs` loads latest session and constructs provider, runtime, and TUI in one process |
| Durable anchor | Session is the main durable execution container | Session is durable, but active execution also depends on `RunRecord` and ephemeral `active_run_id` |
| Input representation | User and assistant content are structured message parts | User and assistant history is represented through `Turn` plus separate run and tool records |
| Orchestration core | Prompt loop owns branching for normal turns, subtasks, and compaction | Reducer owns run lifecycle, approvals, batching barriers, cancellation, and delegation handoff |
| Tool approval | Policy objects and approvals are resumable control flow | App-owned permission policy, reducer resumes after tool replies, built-in `task` is ask-only and not rememberable |
| Delegation | Subtasks create or resume real child sessions | Delegated work creates child runs with durable lineage inside one local session |
| Parent resume | Child terminal state resumes parent from durable session state | Child terminal state synthesizes a parent `ToolExecutionFinished` and re-enters normal batch semantics |
| Streaming | Structured parts stream over an event bus or SSE, publication is transaction-aware | Provider deltas are forwarded through runtime and reducer to the in-process TUI |
| Recovery | Retry, abort, revert, unrevert, and compaction all mutate durable session state | Cancellation and checkpointing are durable, and startup can reconcile one interrupted delegated child, but execution ownership and richer recovery are not fully persisted |

## Major differences and current gaps

1. **Runtime topology**

   OpenCode is built around a server-owned runtime that can coordinate multiple clients and project instances. This repo is still a single local process with an in-process TUI. That keeps the architecture simpler, but it also means there is no server-owned coordination layer, global work queue, or multi-client session ownership.

2. **Durable execution model**

   OpenCode treats session state, session status, and session mutation as the core execution surface. This repo persists session and run data, but active ownership still depends on ephemeral app state such as `active_run_id`, and the runtime explicitly does not own scheduling or persistence.

3. **Structured transcript model**

   OpenCode's comparison baseline is part-based messages, not plain text turns. This repo has durable `Turn`, `RunRecord`, `ToolInvocationRecord`, and `TaskDelegationRecord` data, which is enough for current scheduling and replay, but it is not the same first-class part model.

4. **Recovery breadth**

   OpenCode includes typed status, retry, abort, revert, unrevert, and compaction as durable state transitions. This repo now preserves cancellation, checkpointing, batching barriers, delegated terminal handoff, and one-shot startup reconciliation for a single interrupted delegated child, but it does not yet match that broader recovery surface.

5. **Agent and background execution model**

   OpenCode makes agent taxonomy, agent config, and session-centric async prompt execution explicit. This repo has a real `task` tool contract and delegated-agent lookup, but phase 1 still stops short of background sibling subagents, recursive child delegation, and persisted execution ownership across restart.

## Delegated scheduling parity

Current parity is strongest around delegated scheduling. The local implementation now matches the key OpenCode behavior that matters most for subagent orchestration:

- delegated work creates real child execution lineage, not a nested in-memory helper call
- the parent pauses while child work becomes the foreground run
- child terminal states are durable and typed through `TaskDelegationStatus`
- parent resumption happens through a durable synthetic tool result, which preserves existing batch and replay rules
- parent cancellation semantics cascade through the delegated lifecycle instead of leaving orphaned child work

That parity is still intentionally narrow. As documented in [`opencode-delegated-scheduling.md`](opencode-delegated-scheduling.md), the current repo now persists a minimal foreground owner and can safely recover root generation, awaiting-tool-approval state, and one interrupted delegated child on startup. It still does **not** provide background sibling subagents, recursive child delegation, generic in-flight tool restart, a global work queue, or server-owned multi-client coordination.

## Notes on permissions comparison

Permission policy is closer than several other areas, but comparisons should prefer source-grounded behavior over documentation defaults when they disagree. On the local side, permission ownership is clearly app-owned, centrally evaluated, and resumable through reducer control flow. On the OpenCode side, the grounded summary says the policy model is layered and pattern-based, with resumable approvals.

## Recommended phase-2 ports, ranked

The phase-2 roadmap below is ranked by how directly each item closes the largest architectural mismatch with OpenCode while fitting the grounded current state.

| Rank | Port | Why it ranks here |
|---|---|---|
| 1 | Extend durable execution ownership beyond safe root/approval restore and interrupted-child reconciliation | The repo now persists a minimal foreground owner, but it still cannot generically resume in-flight tool execution or restart child generation from a true child-resume request. |
| 2 | Promote session status and retry to first-class durable execution state | OpenCode is explicitly session-status driven with built-in retry. This repo already has durable run and delegation records, so this is a direct extension of the current app-owned model. |
| 3 | Evolve from turn-plus-record replay toward a richer part-based message model | The current reducer and replay design works, but OpenCode's structured part model is a deeper architectural difference that affects streaming, recovery, and future orchestration branches. |
| 4 | Add session-centric background scheduling beyond single foreground child handoff | The current parity stops at one foreground child lifecycle. OpenCode's async prompt execution model goes further, so this is the next delegation-oriented expansion after restart durability. |
| 5 | Introduce a server-owned coordination layer only after the session model is stronger | OpenCode's client/server shape matters, but porting it early would be premature while execution ownership, status, and message structure are still more limited locally. |

## Practical reading order

Read this document first if the question is, "How close is the current Rust architecture to OpenCode?" Then use:

1. [`../opencode-workflow-report.md`](../opencode-workflow-report.md) for the broader OpenCode workflow baseline
2. [`opencode-delegated-scheduling.md`](opencode-delegated-scheduling.md) for the current phase-1 parity area
3. [`agent-architecture-spec.md`](agent-architecture-spec.md) for the broader target model that goes beyond today's implementation
