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
| `box` | **Task** -- runs Claude Code with the prompt | CodergenHandler | `prompt` |
| `diamond` | **Conditional** -- Claude picks the outgoing edge | ConditionalHandler | `prompt`, `node_type="conditional"` |
| `hexagon` | **Human gate** -- pauses for human approval | WaitHumanHandler | `node_type="wait.human"` |
| `parallelogram` | **Tool** -- runs a shell command | ToolHandler | `tool_command` |

## Node Attributes

| Attribute | Type | Default | Description |
|-----------|------|---------|-------------|
| `label` | string | node ID | Display name in logs |
| `prompt` | string | -- | Task sent to Claude Code. Required for `box` and `diamond`. |
| `shape` | string | -- | Node shape (see table above) |
| `node_type` | string | auto | Handler override: `"conditional"`, `"tool"`, `"parallel"`, `"fan_in"`, `"manager"`, `"wait.human"` |
| `llm_model` | string | graph `model` | Model override: `"haiku"`, `"sonnet"`, `"opus"`, or full model ID |
| `llm_provider` | string | `"claude"` | CLI provider: `"claude"`, `"codex"`, `"gemini"` |
| `allowed_tools` | string | all | Comma-separated tool list, e.g. `"Read,Grep,Glob"` or `"Bash(git:*)"` |
| `max_budget_usd` | string | unlimited | Spend cap for this node's session |
| `goal_gate` | boolean | false | Must succeed for pipeline completion |
| `retry_target` | string | -- | Node to loop back to on goal gate failure |
| `fallback_retry_target` | string | -- | Second-level retry target |
| `max_retries` | integer | 0 | Max retry attempts |
| `timeout` | duration | -- | Max execution time: `120s`, `600s`, `15m`, `1h` |
| `tool_command` | string | -- | Shell command for `parallelogram` nodes |
| `fidelity` | string | -- | Context mode: `"full"`, `"truncate"`, `"compact"`, `"summary"` |
| `classes` | string | -- | Space-separated class list for stylesheet matching |
| `auto_status` | boolean | true | Auto-set status from outcome |
| `allow_partial` | boolean | false | Allow partial success |

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
    label="Implement Feature"
    timeout=900s
    prompt="Implement the feature described in .pas/current_task.md"
]

verify_step [
    shape="diamond"
    label="Verify"
    node_type="conditional"
    timeout=600s
    prompt="Check the implementation. Respond PASS or FAIL on the last line."
]

fixup [
    shape="box"
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

The pipeline engine validates graphs against these rules (run `pas validate <file>`):

1. Exactly one `Mdiamond` (start) node
2. Exactly one `Msquare` (exit) node
3. Start node has no incoming edges
4. Exit node has no outgoing edges
5. All nodes are reachable from start
6. Exit is reachable from all nodes
7. `box` and `diamond` nodes have a `prompt` attribute
8. `parallelogram` nodes have a `tool_command` attribute
9. `diamond` nodes have at least two outgoing edges
10. No orphan nodes (every non-start/exit node has at least one incoming edge)
11. Back-edges in loops should have `loop_restart=true`
12. `timeout` is set on every work node
