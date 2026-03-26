# Unified Agent Architecture Specification

> Synthesized from [oh-my-openagent](https://github.com/code-yeongyu/oh-my-openagent) (OmO) and [oh-my-opencode-slim](https://github.com/alvinunreal/oh-my-opencode-slim) (Slim).

---

## 1. Comparative Analysis

### 1.1 Project Overview

| Dimension | oh-my-openagent (OmO) | oh-my-opencode-slim (Slim) |
|-----------|----------------------|---------------------------|
| **Agent count** | 11 (richly specialized) | 6 (lean and focused) |
| **Hierarchy** | 3-layer (planning / orchestration / workers) | 2-tier (orchestrator / leaf subagents) |
| **Delegation** | Multi-hop (orchestrators can sub-delegate) | Hub-and-spoke (flat, no sub-delegation) |
| **Model routing** | Category-based with provider-aware transforms | Priority-array fallback chains with runtime failover |
| **Tool control** | Allowlist/blocklist permission system | MCP access control + prompt-level constraints |
| **Prompt strategy** | Model-polymorphic (GPT/Gemini/Claude variants) | Append-semantics (base + user extension) |
| **Session mgmt** | `session_id` continuity across invocations | Fire-and-forget with automatic notification |
| **Cost governance** | FREE / CHEAP / EXPENSIVE labels in prompts | Delegation heuristics embedded in orchestrator prompt |
| **Config system** | Category presets (ultrabrain, quick, deep, etc.) | Named presets with per-agent Zod-validated overrides |

### 1.2 Shared Agent Roles

Both projects independently converge on the same core agent archetypes:

| Role | OmO Agent | Slim Agent | Shared Responsibility |
|------|-----------|------------|----------------------|
| Orchestrator | Sisyphus / Atlas | Orchestrator | Parse intent, delegate, verify, ship |
| Code searcher | Explore | Explorer | Read-only codebase grep and discovery |
| Doc researcher | Librarian | Librarian | External docs, APIs, library references |
| Strategic advisor | Oracle | Oracle | Architecture reasoning, debugging counsel |
| Task executor | Sisyphus-Junior / Hephaestus | Fixer | Write code, apply patches, implement changes |
| UI specialist | -- | Designer | UI/UX implementation (creative temperature) |
| Planner | Prometheus | -- | Interview-based requirement elicitation |
| Gap analyzer | Metis | -- | Pre-implementation gap detection |
| Plan reviewer | Momus | -- | Feasibility validation of plans |
| Media analyst | Multimodal-Looker | -- | PDF/image/diagram interpretation |

### 1.3 Key Innovations by Project

**OmO innovations:**
- Intent Gate pattern (Phase 0 verbalization before action)
- Wisdom accumulation across delegation rounds
- Session continuity via `session_id` reuse
- Model-polymorphic prompts (Gemini anti-attention-decay sections)
- Anti-duplication guards (orchestrator won't re-search after delegating)
- Greek mythology naming as behavioral encoding

**Slim innovations:**
- Strict leaf-node enforcement (no sub-delegation chains)
- MCP access as agent identity (Librarian is sole research gateway)
- Runtime rate-limit failover with transparent model switching
- Temperature as a design lever (0.7 for creative agents)
- Fire-and-forget background tasks (~1ms return)
- Preset system for swappable configuration profiles

---

## 2. Unified Agent Type System

### 2.1 Type Categorization

Agents are classified along three orthogonal axes:

```
                    +-----------+
                    |   Agent   |
                    +-----+-----+
                          |
          +---------------+---------------+
          |               |               |
     [Tier]          [Capability]     [Cost Class]
   structural        functional       economic
```

#### Axis 1: Tier (structural role in delegation graph)

| Tier | Description | Delegation Rights | Example |
|------|-------------|-------------------|---------|
| `planner` | Produces plans, cannot execute code | Can spawn advisors only | Prometheus |
| `orchestrator` | Parses intent, delegates, verifies | Can spawn all tiers below | Sisyphus, Atlas, Orchestrator |
| `specialist` | Domain-scoped autonomous worker | Can spawn utility agents only | Hephaestus, Designer, Fixer |
| `utility` | Narrow read-only or single-purpose tool | Leaf node, no delegation | Explorer, Librarian, Oracle, Multimodal-Looker |

**Delegation rules** (directed acyclic):
```
planner -> orchestrator, utility(advisor)
orchestrator -> specialist, utility
specialist -> utility(exploration)
utility -> (none)
```

#### Axis 2: Capability (functional classification)

| Capability | Description | Tool Access | Examples |
|------------|-------------|-------------|----------|
| `exploration` | Codebase search and discovery | Read-only: grep, glob, AST-search, LSP | Explorer, Explore |
| `research` | External documentation and web lookup | MCPs: websearch, context7, grep_app | Librarian |
| `advisory` | Strategic reasoning and review | Read-only: read, glob, grep | Oracle, Metis, Momus |
| `implementation` | Code writing and modification | Full: write, edit, patch, shell | Fixer, Sisyphus-Junior, Hephaestus |
| `creative` | Design-oriented implementation | Full + elevated temperature | Designer |
| `perception` | Media file analysis | Minimal: read only | Multimodal-Looker |
| `orchestration` | Delegation, verification, assembly | Full + delegation tools | Sisyphus, Atlas, Orchestrator |
| `planning` | Requirement gathering and plan authoring | Write to plan directory only | Prometheus |

#### Axis 3: Cost Class (economic governance)

| Class | Token Budget | When to Use | Examples |
|-------|-------------|-------------|---------|
| `free` | Negligible | Always acceptable, fire liberally | Explorer |
| `cheap` | Low | Default for routine tasks | Librarian, Fixer, Multimodal-Looker |
| `standard` | Moderate | Primary orchestration work | Orchestrator, Sisyphus |
| `expensive` | High | Complex reasoning, use sparingly | Oracle, Hephaestus, Atlas |

### 2.2 Agent Mode (runtime context)

Inherited from OmO's `AgentMode` and Slim's SDK modes:

| Mode | Behavior |
|------|----------|
| `primary` | User-facing; respects UI model selection |
| `subagent` | System-invoked; uses own model/fallback chain |
| `dual` | Operates in both contexts (OmO's `"all"` mode) |

---

## 3. Unified Agent Specifications

### 3.1 Planner

```yaml
name: Planner
tier: planner
capability: planning
cost_class: expensive
mode: primary
temperature: 0.2
delegation: [utility(advisory)]

responsibilities:
  - Interview-based requirement elicitation
  - Structured plan document authoring
  - Scope and constraint identification
  - Handoff to orchestrator with complete plan

constraints:
  - MUST NOT write code files
  - MUST NOT execute implementation tools
  - CAN ONLY write to designated plan directories
  - Outputs structured plan documents (markdown)

tools_allowed: [read, glob, grep, write(plan_dir_only)]
tools_denied: [edit, patch, shell, task(specialist)]
```

### 3.2 Orchestrator

```yaml
name: Orchestrator
tier: orchestrator
capability: orchestration
cost_class: standard
mode: dual
temperature: 0.1
delegation: [specialist, utility]

responsibilities:
  - Intent Gate: verbalize interpretation before acting
  - Path analysis (quality / speed / cost / reliability)
  - Parallel delegation of independent tasks
  - Result verification and assembly
  - Wisdom accumulation across delegation rounds
  - Session continuity management

constraints:
  - MUST perform Intent Gate on every user message
  - MUST prefer delegation over self-execution for specialized work
  - MUST NOT re-search after delegating exploration
  - MUST reuse session_id for follow-up to same agent

tools_allowed: [all]
tools_denied: []
```

### 3.3 Explorer

```yaml
name: Explorer
tier: utility
capability: exploration
cost_class: free
mode: subagent
temperature: 0.1
delegation: []

responsibilities:
  - Codebase search via grep, glob, AST patterns
  - File discovery and path resolution
  - Returns file paths with relevant snippets
  - Answers "where is X?" questions

constraints:
  - READ-ONLY: no file modifications
  - No delegation to other agents
  - Returns structured search results, not analysis

tools_allowed: [read, glob, grep, ast_grep_search, lsp]
tools_denied: [write, edit, patch, task, shell, mcp(*)]
```

### 3.4 Librarian

```yaml
name: Librarian
tier: utility
capability: research
cost_class: cheap
mode: subagent
temperature: 0.1
delegation: []

responsibilities:
  - External documentation lookup
  - Library API reference retrieval
  - GitHub code search for usage examples
  - Web search for solutions and patterns

constraints:
  - READ-ONLY: no file modifications
  - No delegation to other agents
  - Sole gateway to external research MCPs

tools_allowed: [read, glob, grep, mcp(websearch), mcp(context7), mcp(grep_app)]
tools_denied: [write, edit, patch, task, shell]
```

### 3.5 Oracle

```yaml
name: Oracle
tier: utility
capability: advisory
cost_class: expensive
mode: subagent
temperature: 0.1
delegation: []

responsibilities:
  - Deep architectural reasoning
  - Complex debugging and root-cause analysis
  - Code review and design feedback
  - Trade-off analysis for technical decisions

constraints:
  - READ-ONLY: advises, never implements
  - No delegation to other agents
  - Use sparingly due to high cost

tools_allowed: [read, glob, grep]
tools_denied: [write, edit, patch, task, shell, mcp(*)]
```

### 3.6 Implementer

```yaml
name: Implementer
tier: specialist
capability: implementation
cost_class: cheap
mode: subagent
temperature: 0.2
delegation: [utility(exploration)]

responsibilities:
  - Execute code changes from complete context
  - Apply patches and modifications
  - Run tests and validate changes
  - Fast, focused task execution

constraints:
  - No external research (no MCPs)
  - No orchestration-level delegation
  - Receives complete context; does not self-research
  - CAN spawn Explorer for targeted lookups

tools_allowed: [read, write, edit, patch, shell, glob, grep, task(explorer)]
tools_denied: [mcp(*), task(oracle), task(librarian)]
```

### 3.7 Designer

```yaml
name: Designer
tier: specialist
capability: creative
cost_class: cheap
mode: subagent
temperature: 0.7
delegation: []

responsibilities:
  - UI/UX implementation
  - Visual design decisions (typography, color, spacing, motion)
  - Frontend component creation and styling
  - Creative problem-solving for user-facing interfaces

constraints:
  - Higher temperature for creative output
  - No external research access
  - No delegation to other agents

tools_allowed: [read, write, edit, patch, shell, glob, grep]
tools_denied: [mcp(*), task(*)]
```

### 3.8 Deep Worker

```yaml
name: DeepWorker
tier: specialist
capability: implementation
cost_class: expensive
mode: dual
temperature: 0.2
delegation: [utility(exploration), utility(research)]

responsibilities:
  - Autonomous end-to-end implementation of complex tasks
  - Self-directed research and exploration
  - Multi-file refactoring and feature development
  - Quality-first execution with built-in verification

constraints:
  - Use for complex tasks that justify the cost
  - Can self-delegate to exploration and research utilities
  - Should not be used for simple, well-scoped changes

tools_allowed: [all]
tools_denied: [task(orchestrator), task(planner)]
```

### 3.9 Gap Analyzer

```yaml
name: GapAnalyzer
tier: utility
capability: advisory
cost_class: standard
mode: subagent
temperature: 0.1
delegation: []

responsibilities:
  - Pre-implementation gap detection
  - Identify missing requirements, edge cases, and risks
  - Surface implicit assumptions in plans
  - Produce structured gap reports

constraints:
  - READ-ONLY: analysis only
  - Invoked between planning and execution phases
  - No delegation

tools_allowed: [read, glob, grep]
tools_denied: [write, edit, patch, task, shell, mcp(*)]
```

### 3.10 Plan Reviewer

```yaml
name: PlanReviewer
tier: utility
capability: advisory
cost_class: standard
mode: subagent
temperature: 0.1
delegation: []

responsibilities:
  - Validate feasibility of proposed plans
  - Check for logical consistency and completeness
  - Assess effort estimates and risk factors
  - Approve or reject with structured feedback

constraints:
  - READ-ONLY: review only
  - Produces structured accept/reject/revise verdicts
  - No delegation

tools_allowed: [read, glob, grep]
tools_denied: [write, edit, patch, task, shell, mcp(*)]
```

### 3.11 Perception Agent

```yaml
name: Perceiver
tier: utility
capability: perception
cost_class: cheap
mode: subagent
temperature: 0.1
delegation: []

responsibilities:
  - Analyze images, PDFs, diagrams, and media files
  - Extract structured information from visual content
  - Describe UI screenshots for other agents

constraints:
  - Most restricted tool access
  - Read-only, single file at a time
  - No delegation

tools_allowed: [read]
tools_denied: [write, edit, patch, task, shell, glob, grep, mcp(*)]
```

---

## 4. Delegation Architecture

### 4.1 Delegation Graph

```
                         +-----------+
                         |  Planner  |
                         +-----+-----+
                               |
                    spawns plans for
                               |
                               v
                      +--------+--------+
                      |  Orchestrator   |<-------- user messages
                      +--------+--------+
                               |
              +-------+--------+--------+-------+
              |       |        |        |       |
              v       v        v        v       v
         Explorer  Librarian Oracle  Implementer Designer
                                        |
                                        v
                                    Explorer
                                  (sub-lookup)
```

### 4.2 Delegation Protocol

Every delegation call includes:

```typescript
interface DelegationRequest {
  agent: AgentName;           // target agent
  prompt: string;             // task description with full context
  description: string;        // short label for tracking
  session_id?: string;        // reuse for follow-up (OmO pattern)
  run_in_background: boolean; // true for parallel, false for blocking
  cost_hint?: CostClass;      // budget guidance
}
```

### 4.3 Delegation Principles

1. **Intent Gate first.** The orchestrator verbalizes its interpretation of the user's request before any delegation.
2. **Parallel by default.** Independent tasks are dispatched concurrently. Sequential only when outputs feed inputs.
3. **Anti-duplication.** Once exploration is delegated, the orchestrator must not perform redundant searches.
4. **Session continuity.** Follow-up interactions with the same agent reuse `session_id` to preserve context.
5. **Wisdom accumulation.** The orchestrator categorizes learnings (conventions, failures, gotchas) from each delegation round and forwards them to subsequent agents.
6. **Leaf nodes are terminal.** Utility-tier agents cannot delegate. Specialist-tier agents can only delegate downward to utility agents.
7. **Cost-aware selection.** Prefer `free` and `cheap` agents; escalate to `expensive` only when justified.

---

## 5. Cross-Cutting Concerns

### 5.1 Model Resolution Pipeline

```
UI selection (if primary mode)
  -> user per-agent override
    -> preset profile
      -> agent default model
        -> fallback chain (priority-ordered)
          -> provider-aware transform
```

At each level, the first available model wins. Runtime rate-limit errors trigger automatic failover to the next model in the chain (Slim's `ForegroundFallbackManager` pattern).

### 5.2 Tool Permission Model

Permissions are enforced at two layers:

| Layer | Mechanism | Granularity |
|-------|-----------|-------------|
| **Hard** | SDK allowlist/blocklist per agent | Per-tool function |
| **Soft** | System prompt constraints | Behavioral guidance |

Hard permissions are non-bypassable. Soft permissions guide well-behaved models but are not enforced at the runtime level. Both layers should agree; when they diverge, the hard layer is authoritative.

### 5.3 Prompt Architecture

Each agent's prompt is assembled from composable sections:

```
[Base prompt]                 -- core identity and responsibilities
[Model-specific overrides]    -- Gemini anti-attention-decay, etc.
[Dynamic capability table]    -- available agents, tools, costs at runtime
[Injected skills]             -- git-master, playwright, etc.
[User append section]         -- custom extensions without replacing base
[Anti-pattern guards]         -- anti-duplication, delegation limits
```

### 5.4 Background Task Lifecycle

```
Orchestrator                     Subagent
     |                                |
     |--- background_task(agent, prompt) -->|
     |<-- task_id (~1ms) ---------|        |
     |                            |  (executing...)
     |  (continues other work)    |        |
     |                            |  (completes)
     |<-- system notification ----|        |
     |--- background_output(id) -------->  |
     |<-- result ------------------|
```

Maximum concurrent background tasks: configurable (default 10).

---

## 6. Configuration Schema

```typescript
interface AgentSystemConfig {
  // Per-agent overrides
  agents: Record<AgentName, {
    model?: string | string[];      // single or priority-ordered fallback
    temperature?: number;
    variant?: string;
    skills?: string[];              // injected skill prompts
    mcps?: string[];                // MCP access (wildcard/exclusion)
    prompt_append?: string;         // additive prompt extension
  }>;

  // Named preset profiles
  presets: Record<string, Partial<AgentSystemConfig['agents']>>;
  active_preset?: string;

  // Global fallback chains
  fallback: {
    chains: Record<string, string[]>;  // model -> [fallback1, fallback2]
    auto_failover: boolean;            // runtime rate-limit switching
  };

  // Delegation limits
  delegation: {
    max_concurrent_background: number;  // default: 10
    max_delegation_depth: number;       // default: 2
    timeout_ms: number;                 // per-task timeout
  };

  // Cost governance
  cost: {
    prefer_cheap: boolean;              // bias toward free/cheap agents
    expensive_confirmation: boolean;    // prompt user before expensive agents
  };
}
```

---

## 7. Design Principles

1. **Separation of concerns over flexibility.** Each agent has a narrow, well-defined scope. Broad agents are split into multiple focused ones.

2. **Read-only by default.** Agents that don't need write access don't get it. This is the single most effective guard against unintended side effects.

3. **Flat over deep delegation.** Prefer hub-and-spoke (Slim) for predictability. Allow limited sub-delegation (OmO) only when a specialist genuinely needs utility support.

4. **Cost transparency.** Every agent carries a cost class. Orchestrators see this in their capability tables and factor it into delegation decisions.

5. **Model-agnostic agent definitions.** Agent identity is defined by role and constraints, not by which model runs it. Model assignment is a separate configuration concern.

6. **Prompt composability.** Prompts are assembled from independent sections. No monolithic prompt strings. Sections can be conditionally included based on runtime capabilities.

7. **Graceful degradation.** If a model is unavailable, the fallback chain activates transparently. If an agent fails, the orchestrator retries with an alternative agent or self-executes.

8. **Session awareness.** Agent conversations are not one-shot. Reusing session context across delegation rounds saves tokens and preserves reasoning continuity.
