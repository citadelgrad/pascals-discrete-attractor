# Attractor DOT Dialect Reference

Attractor pipelines use a **strict subset** of the Graphviz DOT language with custom extensions. This document is the authoritative reference for what the `attractor-dot` parser accepts. Code that generates DOT for attractor (including `pas generate`) must conform to these rules.

## Grammar

Only directed graphs are supported. The parser rejects `graph`, `strict`, and `--` edges.

```
digraph  : 'digraph' NAME '{' stmt* '}'

stmt     : graph_attr    -- graph [ attr_block ]
         | node_default  -- node [ attr_block ]
         | edge_default  -- edge [ attr_block ]
         | subgraph      -- subgraph NAME? { stmt* }
         | node_stmt     -- NAME [ attr_block ]?
         | edge_stmt     -- NAME ( '->' NAME )+ [ attr_block ]?
         | decl          -- NAME '=' VALUE

NAME     : [A-Za-z_][A-Za-z0-9_]*

attr_block : '[' ( KEY '=' VALUE ( [,;]? KEY '=' VALUE )* )? ']'

KEY      : NAME ( '.' NAME )*          -- dotted keys allowed (e.g. style.model)
```

## Identifiers (NAME)

Must start with an ASCII letter or underscore, followed by ASCII alphanumerics or underscores.

| Valid | Invalid | Why |
|-------|---------|-----|
| `my_node` | `"my node"` | Quoted IDs not supported |
| `step_1` | `1step` | Cannot start with a digit |
| `nodeA` | `42` | Numeric-only IDs not supported |
| `cluster_main` | `node:port` | Port syntax not supported |

Use `snake_case` for node IDs. Keep them short and descriptive.

## Attribute Values

The parser recognizes five value types, tried in this order:

| Type | Syntax | Examples |
|------|--------|----------|
| **String** | Double-quoted | `"hello"`, `"line1\nline2"` |
| **Boolean** | Bare literal | `true`, `false` |
| **Duration** | Integer + suffix (unquoted) | `120s`, `250ms`, `15m`, `2h`, `7d` |
| **Float** | Digits `.` digits | `1.5`, `-0.75` |
| **Integer** | Optional sign + digits | `42`, `-3`, `+10` |

Semantic discriminator attributes must be strings even though the dialect also
supports typed scalar values. Quote values for `shape`, `type`, `node_type`,
`handler`, `prompt`, `llm_provider`, `class`, `classes`, `stylesheet`, and
`model_stylesheet`. A non-string value such as `shape=123` or `prompt=true` is
an `InvalidAttributeType` compilation error; PAS does not discard it and infer
an executable default.

### Strings

- Delimited by double quotes: `"content"`
- Escape sequences: `\n` (newline), `\t` (tab), `\\` (backslash), `\"` (quote)
- Can span multiple lines (the newlines are literal)
- Unrecognized escapes like `\x` are kept verbatim as `\x`

### Duration (attractor extension)

Not part of standard Graphviz. Unquoted integer followed by a time suffix:

| Suffix | Meaning | Example |
|--------|---------|---------|
| `ms` | milliseconds | `250ms` |
| `s` | seconds | `120s` |
| `m` | minutes | `15m` |
| `h` | hours | `2h` |
| `d` | days | `7d` |

Quoted durations (e.g. `"120s"`, `"5m"`) parse as strings, not Duration values. The engine handles both forms, but prefer unquoted for clarity.

### Dotted keys (attractor extension)

Attribute keys can use dots for namespacing: `style.model`, `config.max_retries`. Not part of standard Graphviz.

## Attribute Separators

Inside `[ ]` blocks, attributes can be separated by commas, semicolons, or just whitespace:

```dot
// All equivalent:
node_a [label="A", shape="box", timeout=600s]
node_a [label="A"; shape="box"; timeout=600s]
node_a [label="A" shape="box" timeout=600s]
```

Only one `[ ]` block per statement is consumed. Chained blocks (`[a=1][b=2]`) are **not** supported.

## Comments

```dot
// Line comment (to end of line)
/* Block comment
   (may span multiple lines) */
```

`#` preprocessor comments are **not** supported.

Comments inside strings are preserved verbatim (not treated as comments).

## Default Blocks

Set defaults for all subsequent nodes or edges in the current scope:

```dot
node [shape="box", timeout=600s]    // all nodes below get these defaults
edge [color="gray"]                 // all edges below get these defaults
graph [label="My Pipeline"]         // graph-level attributes
```

Defaults propagate into subgraphs. Subgraph-level defaults override parent defaults.

## Subgraphs

```dot
subgraph my_group {
    a -> b -> c
}

// Anonymous (no name)
subgraph {
    x -> y
}
```

Subgraph names follow the same ID rules (bare identifiers only). The `cluster_` prefix has no special semantic meaning to the attractor parser (unlike Graphviz renderers).

## Edge Chains

Chained edges expand into pairwise edges sharing the same attributes:

```dot
// This:
a -> b -> c [label="flow"]

// Becomes two edges:
//   a -> b [label="flow"]
//   b -> c [label="flow"]
```

Nodes referenced in edges are implicitly created (with current node defaults) if not explicitly declared.

## NOT Supported

These standard Graphviz DOT features will cause parse errors or be silently ignored:

| Feature | Status |
|---------|--------|
| Undirected graphs (`graph G { }`) | **Parse error** |
| Undirected edges (`a -- b`) | **Parse error** |
| `strict` keyword | **Parse error** |
| Quoted node IDs (`"my node"`) | **Parse error** |
| Numeric node IDs (`42 -> 99`) | **Parse error** |
| HTML labels (`<B>text</B>`) | **Parse error** |
| Port syntax (`node:port:compass`) | **Parse error** |
| String concatenation (`"a" + "b"`) | **Parse error** |
| Subgraph as edge endpoint (`{a b} -> c`) | **Parse error** |
| Chained attr blocks (`[a=1][b=2]`) | Second block ignored |
| `#` preprocessor comments | Not recognized |
| Floats without leading digit (`.5`) | **Parse error** (use `0.5`) |
| Scientific notation (`1e-3`) | **Parse error** |

---

# Pipeline Semantics

The grammar above defines what **parses**. This section defines what the attractor pipeline **engine** does with the parsed graph.

## Node Shapes and Handlers

| Shape | Role | Handler | Required Attributes |
|-------|------|---------|---------------------|
| `Mdiamond` | **Start** -- entry point, exactly one | StartHandler | none |
| `Msquare` | **Exit** -- pipeline completion, exactly one | ExitHandler | none |
| `box` | **Task** -- runs the selected provider CLI; `prompt` is optional | CodergenHandler | `llm_provider` |
| `diamond` with a prompt | **LLM conditional** -- provider output picks the outgoing edge | CodergenHandler | `prompt`, `llm_provider` |
| `diamond` without a prompt or explicit `type="codergen"` | **Conditional** -- pass-through routing | ConditionalHandler | none |
| `diamond` with explicit `type="codergen"` | **LLM conditional** -- provider output picks the outgoing edge, even when `prompt` is absent | CodergenHandler | `llm_provider`; `prompt` is optional |
| `hexagon` | **Human gate** -- pauses for human approval | WaitHumanHandler | none |
| `parallelogram` | **Tool** -- runs a shell command | ToolHandler | `tool_command` |

## Node Attributes

| Attribute | Type | Default | Description |
|-----------|------|---------|-------------|
| `label` | string | node ID | Display name in logs |
| `prompt` | string | -- | Task sent to the selected provider CLI. Its presence makes a conditional LLM-backed; explicit `type="codergen"` does so even without a prompt. |
| `shape` | string | -- | Node shape (see table above) |
| `type` | string | auto | Handler override: `"codergen"`, `"conditional"`, `"tool"`, `"parallel"`, `"fan_in"`, `"manager"`, `"quality"`, `"wait.human"` |
| `llm_model` | string | graph `model` | Model override: `"haiku"`, `"sonnet"`, `"opus"`, or full model ID |
| `llm_provider` | string | -- | Required whenever the resolved handler consumes a provider. Values: `"claude"`, `"codex"`, `"gemini"`; aliases: `anthropic`, `openai`, `google` (case-insensitive). |
| `allowed_tools` | string | all | Comma-separated tool list, e.g. `"Read,Grep,Glob"` or `"Bash(git:*)"` |
| `max_budget_usd` | string | unlimited | Spend cap for this node's session |
| `goal_gate` | boolean | false | Must succeed for pipeline completion |
| `retry_target` | string | -- | Node to loop back to on goal gate failure |
| `fallback_retry_target` | string | -- | Second-level retry target |
| `max_retries` | integer | 0 | Max retry attempts |
| `timeout` | duration | -- | Max execution time: `120s`, `600s`, `15m`, `1h` |
| `tool_command` | string | -- | Shell command for `parallelogram` nodes |
| `fidelity` | string | -- | Context mode: `"full"`, `"truncate"`, `"compact"`, `"summary"` |
| `class` | string | -- | Space-separated class list for stylesheet matching |
| `auto_status` | boolean | true | Auto-set status from outcome |
| `allow_partial` | boolean | false | Allow partial success |

Compatibility aliases are accepted at the semantic compilation boundary: `node_type` or
`handler` for `type`, `stylesheet` for `model_stylesheet`, and `classes` for `class`.
If canonical and compatibility spellings are both present with different values,
compilation fails with a typed `ConflictingAttributeAliases` diagnostic.

## Canonical semantic compilation

After parsing and DOT-default resolution, PAS normalizes aliases, applies stylesheets,
expands prompt variables, and compiles one immutable `ExecutionPlan`. Validation,
preflight, handler dispatch, provider selection, start/exit recognition, and execution
all consume that plan. They do not independently reinterpret shape or provider strings.

Runtime compilation is fail-closed. Unknown shapes without an explicit registered
handler, unknown handlers/providers, conflicting role signals, missing providers on
`codergen` nodes, and ambiguous start/exit cardinality prevent execution before logs,
checkpoints, handlers, or provider CLIs start. `pas generate` and `pas scaffold` may
insert an explicit Claude provider into generated source, then recompile it strictly.
A registered custom handler may use an omitted or otherwise unknown custom shape; it
cannot override a known built-in role shape without producing
`ConflictingRoleSignals`.

Exactly one start and one exit are required. `shape="Mdiamond"` and
`shape="Msquare"` are canonical. For compatibility, a node whose shape and type are
both omitted may use case-insensitive ID `start` for start or `exit`, `end`, or `done`
for exit. Combining a magic ID with incompatible shape/type signals is an error.

## Edge Attributes

| Attribute | Type | Default | Description |
|-----------|------|---------|-------------|
| `label` | string | -- | Display label and preferred_label matching |
| `condition` | string | -- | Condition expression, e.g. `"preferred_label=PASS"`, `"outcome=success"` |
| `weight` | integer | 0 | Higher = preferred when multiple edges match |
| `loop_restart` | boolean | false | Clear completed nodes/outcomes (for back-edges in loops) |
| `fidelity` | string | -- | Override fidelity when traversing this edge |

## Graph Attributes

| Attribute | Type | Description |
|-----------|------|-------------|
| `label` | string | Pipeline display name |
| `goal` | string | Pipeline goal description (used by goal gates) |
| `model` | string | Default LLM model for all nodes |

## Common Pipeline Patterns

### Work + verify loop

```dot
work_step [
    shape="box"
    llm_provider="claude"
    label="Implement Feature"
    timeout=900s
    prompt="Implement the feature described in .pas/current_task.md"
]

verify_step [
    shape="diamond"
    llm_provider="claude"
    label="Verify"
    node_type="conditional"
    timeout=600s
    prompt="Check the implementation. Respond PASS or FAIL on the last line."
]

fixup [
    shape="box"
    llm_provider="claude"
    label="Fix Issues"
    timeout=600s
    prompt="Fix the problems found during verification."
]

work_step -> verify_step
verify_step -> next_step [label="PASS", condition="preferred_label=PASS"]
verify_step -> fixup [label="FAIL", condition="preferred_label=FAIL"]
fixup -> verify_step [loop_restart=true]
```

### Tool node (shell command)

```dot
run_tests [
    shape="parallelogram"
    label="Run Tests"
    timeout=300s
    tool_command="cargo test --workspace"
]
```

### Human gate (use sparingly)

Only for decisions that genuinely require human judgment -- not for automatable checks:

```dot
design_review [
    shape="hexagon"
    label="Design Review"
    node_type="wait.human"
    prompt="Review the proposed architecture. Approve to continue or reject to revise."
]
```

### Commit step (required as final work node)

```dot
commit_changes [
    shape="box"
    llm_provider="claude"
    label="Commit Changes"
    timeout=120s
    allowed_tools="Bash(git:*)"
    prompt="Stage and commit all changes made by this pipeline.
1. Run git diff --stat to review what changed
2. Stage the changed files: git add -A
3. Commit with a descriptive message"
]
```

## Validation Rules

`pas validate <file>` first compiles canonical semantics, then applies nine
structural checks. The enforced contract is:

1. Semantic compilation requires exactly one canonical start and one canonical exit.
2. Shape, type/handler aliases, and magic-ID role signals must be compatible.
3. Semantic discriminator attributes have their documented string type; malformed typed values fail closed.
4. Handlers and providers must be known; every provider-consuming handler requires an explicit `llm_provider`.
5. The start has no incoming edge, and the exit has no outgoing edge.
6. Every node is reachable from the start, and every edge target exists.
7. Edge conditions parse correctly.
8. Fidelity values use a supported prefix (`full`, `truncate`, `compact`, or `summary`).
8. Retry targets name existing nodes, and goal gates name a retry target.
9. A `codergen` node should have a prompt or a descriptive label.

The first six items are errors when violated. Fidelity, retry-target, goal-gate,
and prompt findings are warnings. Missing timeouts and provider cost limitations
are runtime preflight warnings, not static validation errors. The validator does
not currently enforce tool-command cardinality, conditional fan-out cardinality,
exit reachability from every node, or `loop_restart` guidance.
