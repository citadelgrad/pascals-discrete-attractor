# Accept & Execute — C4 Diagrams (Graphviz DOT)

## Level 1: System Context

```graphviz
digraph C4_Context {
    graph [rankdir=TB, fontname="Helvetica", fontsize=12, nodesep=1.0, ranksep=1.0, pad=0.5]
    node  [fontname="Helvetica", fontsize=11, shape=box, style="rounded,filled", margin="0.3,0.2"]
    edge  [fontname="Helvetica", fontsize=9, color="#707070"]

    user [label="Developer", shape=box, style="rounded,filled", fillcolor="#08427b", fontcolor=white]

    web  [label="PAS Web\n[Leptos WASM + Axum SSR]", fillcolor="#1168bd", fontcolor=white]
    cli  [label="PAS CLI\n[decompose & scaffold]",    fillcolor="#999999", fontcolor=white]
    fs   [label="Filesystem\n[specs, pipelines, issues]",   fillcolor="#999999", fontcolor=white, shape=cylinder]

    user -> web [label="Approve & Execute"]
    web -> user [label="SSE progress stream", style=dashed]
    web -> cli  [label="child process"]
    web -> fs   [label="read / write"]
    cli -> fs   [label="write .dot"]
}
```

## Level 2: Container

```graphviz
digraph C4_Container {
    graph [rankdir=TB, fontname="Helvetica", fontsize=12, nodesep=1.0, ranksep=1.2, pad=0.5, compound=true]
    node  [fontname="Helvetica", fontsize=11, shape=box, style="rounded,filled", margin="0.3,0.2"]
    edge  [fontname="Helvetica", fontsize=9, color="#707070"]

    user [label="Developer", fillcolor="#08427b", fontcolor=white]

    subgraph cluster_web {
        label="PAS Web"
        style=dashed
        color="#1168bd"
        fontname="Helvetica"
        fontsize=12
        fontcolor="#1168bd"

        wasm      [label="Browser Client\n[Leptos WASM, xterm.js]",  fillcolor="#1168bd", fontcolor=white]
        axum      [label="Axum Server\n[Leptos SSR]",                fillcolor="#1168bd", fontcolor=white]
        broadcast [label="Broadcast Channels\n[tokio, in-memory]",   fillcolor="#438dd5", fontcolor=white, shape=cylinder]
    }

    cli [label="PAS CLI", fillcolor="#999999", fontcolor=white]
    fs  [label="Filesystem",    fillcolor="#999999", fontcolor=white, shape=cylinder]

    user      -> wasm      [label="click button"]
    wasm      -> axum      [label="POST server fn"]
    axum      -> wasm      [label="HTTP + SSE", style=dashed]
    axum      -> cli       [label="tokio::process"]
    axum      -> fs        [label="std::fs"]
    axum      -> broadcast [label="publish_event()"]
    broadcast -> axum      [label="BroadcastStream", style=dashed]
}
```

### Container Legend

| Container | Technology | Responsibility |
|-----------|-----------|----------------|
| Browser Client | Leptos WASM, xterm.js | Renders UI, dispatches server fns, subscribes to SSE |
| Axum Server | Rust, Leptos SSR | Handles server fns, spawns CLI processes, streams events |
| Broadcast Channels | tokio::broadcast | Session-keyed event bus with 100-event replay buffer |
| PAS CLI | Rust binary | `decompose` specs into epics, `scaffold` pipeline .dot files |
| Filesystem | Local disk | `.pas/spec.md`, `pipelines/*.dot`, beads issues |

## Level 3: Component

```graphviz
digraph C4_Component {
    graph [rankdir=TB, fontname="Helvetica", fontsize=12, nodesep=0.8, ranksep=1.0, pad=0.5, compound=true]
    node  [fontname="Helvetica", fontsize=10, shape=box, style="rounded,filled", margin="0.25,0.15"]
    edge  [fontname="Helvetica", fontsize=9, color="#707070"]

    subgraph cluster_browser {
        label="Browser (WASM)"
        style=dashed
        color="#1168bd"
        fontname="Helvetica"
        fontsize=11
        fontcolor="#1168bd"

        approval   [label="ApprovalBar",    fillcolor="#1168bd", fontcolor=white]
        exec_panel [label="ExecutionPanel",  fillcolor="#1168bd", fontcolor=white]
        exec_node  [label="ExecutionNode",   fillcolor="#1168bd", fontcolor=white]
    }

    subgraph cluster_server {
        label="Axum Server (SSR)"
        style=dashed
        color="#438dd5"
        fontname="Helvetica"
        fontsize=11
        fontcolor="#438dd5"

        start_exec [label="start_execution()",    fillcolor="#438dd5", fontcolor=white]
        cli_path   [label="pas_cli_path()",  fillcolor="#438dd5", fontcolor=white]
        runner     [label="PipelineRunner",        fillcolor="#438dd5", fontcolor=white]
        stream     [label="stream_events()",       fillcolor="#438dd5", fontcolor=white]
        publish    [label="publish_event()",       fillcolor="#438dd5", fontcolor=white]
        replay     [label="SESSION_STATE\n[replay buffer]", fillcolor="#438dd5", fontcolor=white, shape=cylinder]
    }

    cli      [label="PAS CLI",    fillcolor="#999999", fontcolor=white]
    registry [label="Handler Registry",  fillcolor="#999999", fontcolor=white]

    approval   -> start_exec [label="POST"]
    start_exec -> cli_path   [label="resolve path"]
    start_exec -> cli        [label="decompose / scaffold"]
    start_exec -> runner     [label="tokio::spawn"]
    start_exec -> approval   [label="ExecutionResponse", style=dashed]
    approval   -> exec_panel [label="on_approve(session_id)"]
    exec_panel -> stream     [label="SSE EventSource"]
    exec_panel -> exec_node  [label="renders"]
    runner     -> registry   [label="resolve + execute"]
    runner     -> publish    [label="events"]
    publish    -> replay     [label="buffer"]
    stream     -> replay     [label="reconnect replay", style=dashed]
    stream     -> exec_panel [label="SSE events", style=dashed]
}
```

### Component Legend

| Component | File | Responsibility |
|-----------|------|----------------|
| ApprovalBar | `components/approval_bar.rs` | Button, dispatches `start_execution()`, shows phase spinner |
| ExecutionPanel | `components/execution_panel.rs` | Subscribes to SSE, renders node list with live status |
| ExecutionNode | `components/execution_node.rs` | Single node: status badge, cost, notes |
| start_execution() | `server/execute.rs` | Orchestrates decompose, scaffold, parse, spawn |
| pas_cli_path() | `server/execute.rs` | Resolves CLI binary from `PAS_CLI_PATH` env var |
| PipelineRunner | `server/execute.rs` | Walks graph, executes handlers, publishes events |
| stream_events() | `server/stream.rs` | SSE endpoint, bridges broadcast to EventSource |
| publish_event() | `server/stream.rs` | Writes to broadcast channel + replay buffer |
| SESSION_STATE | `server/stream.rs` | Last 100 events per session for reconnect replay |

## Sequence (as DOT)

```graphviz
digraph Sequence {
    graph [rankdir=LR, fontname="Helvetica", fontsize=12, nodesep=0.6, ranksep=0.5, pad=0.5]
    node  [fontname="Helvetica", fontsize=10, shape=box, style="rounded,filled", fillcolor="#f0f0f0", margin="0.2,0.1"]
    edge  [fontname="Helvetica", fontsize=8, color="#707070"]

    // Actors / participants
    User       [label="Developer",        fillcolor="#08427b", fontcolor=white]
    AB         [label="ApprovalBar",       fillcolor="#1168bd", fontcolor=white]
    SE         [label="start_execution()", fillcolor="#438dd5", fontcolor=white]
    CLI        [label="PAS CLI",     fillcolor="#999999", fontcolor=white]
    FS         [label="Filesystem",        fillcolor="#999999", fontcolor=white, shape=cylinder]
    PR         [label="PipelineRunner",    fillcolor="#438dd5", fontcolor=white]
    BC         [label="Broadcast",         fillcolor="#438dd5", fontcolor=white, shape=cylinder]
    SSE        [label="stream_events()",   fillcolor="#438dd5", fontcolor=white]
    EP         [label="ExecutionPanel",    fillcolor="#1168bd", fontcolor=white]

    // Flow
    User -> AB  [label="1. Click"]
    AB   -> SE  [label="2. POST server fn"]
    SE   -> FS  [label="3. Check spec.md"]
    SE   -> CLI [label="4. decompose"]
    CLI  -> SE  [label="5. epic_id", style=dashed]
    SE   -> CLI [label="6. scaffold"]
    CLI  -> FS  [label="7. write .dot"]
    SE   -> FS  [label="8. read .dot"]
    SE   -> PR  [label="9. tokio::spawn"]
    SE   -> AB  [label="10. ExecutionResponse", style=dashed]
    AB   -> EP  [label="11. on_approve"]
    EP   -> SSE [label="12. EventSource"]

    // Pipeline loop
    PR  -> BC   [label="13. node_start"]
    BC  -> SSE  [label="14. broadcast", style=dashed]
    SSE -> EP   [label="15. SSE event", style=dashed]
    PR  -> PR   [label="16. execute handler"]
    PR  -> BC   [label="17. node_complete"]
    BC  -> SSE  [label="18. broadcast", style=dashed]
    SSE -> EP   [label="19. SSE event", style=dashed]

    // Completion
    PR  -> BC   [label="20. pipeline_complete"]
    BC  -> SSE  [label="21. broadcast", style=dashed]
    SSE -> EP   [label="22. Done", style=dashed]
}
```
