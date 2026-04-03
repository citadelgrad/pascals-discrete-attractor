# Accept & Execute — C4 Diagrams

## Level 1: System Context

```mermaid
flowchart TB
    user["👤 Developer"]

    web["PAS Web<br/><i>Leptos WASM + Axum SSR</i>"]
    cli["PAS CLI<br/><i>decompose & scaffold</i>"]
    fs[("Filesystem<br/><i>specs, pipelines, issues</i>")]

    user -- "Approve & Execute" --> web
    web -- "SSE progress stream" --> user
    web -- "child process" --> cli
    web -- "read/write" --> fs
    cli -- "write .dot" --> fs

    style user fill:#08427b,color:#fff,stroke:#08427b
    style web fill:#1168bd,color:#fff,stroke:#1168bd
    style cli fill:#999,color:#fff,stroke:#999
    style fs fill:#999,color:#fff,stroke:#999
```

## Level 2: Container

```mermaid
flowchart TB
    user["👤 Developer"]

    subgraph web ["PAS Web"]
        wasm["Browser Client<br/><i>Leptos WASM, xterm.js</i>"]
        axum["Axum Server<br/><i>Leptos SSR</i>"]
        broadcast[("Broadcast Channels<br/><i>tokio, in-memory</i>")]
    end

    cli["PAS CLI"]
    fs[("Filesystem")]

    user -- "click button" --> wasm
    wasm -- "POST server fn" --> axum
    axum -- "HTTP + SSE" --> wasm
    axum -- "tokio::process" --> cli
    axum -- "std::fs" --> fs
    axum -- "publish_event()" --> broadcast
    broadcast -- "BroadcastStream" --> axum

    style user fill:#08427b,color:#fff,stroke:#08427b
    style wasm fill:#1168bd,color:#fff,stroke:#1168bd
    style axum fill:#1168bd,color:#fff,stroke:#1168bd
    style broadcast fill:#438dd5,color:#fff,stroke:#438dd5
    style cli fill:#999,color:#fff,stroke:#999
    style fs fill:#999,color:#fff,stroke:#999
    style web fill:none,stroke:#1168bd,stroke-dasharray:5 5
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

```mermaid
flowchart TB
    subgraph browser ["Browser (WASM)"]
        approval["ApprovalBar"]
        exec_panel["ExecutionPanel"]
        exec_node["ExecutionNode"]
    end

    subgraph server ["Axum Server (SSR)"]
        start_exec["start_execution()"]
        cli_path["pas_cli_path()"]
        runner["PipelineRunner"]
        stream["stream_events()"]
        publish["publish_event()"]
        replay[("SESSION_STATE<br/><i>replay buffer</i>")]
    end

    cli["PAS CLI"]
    registry["Handler Registry"]

    approval -- "POST" --> start_exec
    start_exec --> cli_path
    start_exec -- "decompose / scaffold" --> cli
    start_exec -- "tokio::spawn" --> runner
    start_exec -- "ExecutionResponse" --> approval
    approval -- "on_approve(session_id)" --> exec_panel
    exec_panel -- "SSE EventSource" --> stream
    exec_panel --> exec_node
    runner -- "resolve + execute" --> registry
    runner --> publish
    publish --> replay
    stream -- "reconnect replay" --> replay
    stream -- "SSE events" --> exec_panel

    style approval fill:#1168bd,color:#fff,stroke:#1168bd
    style exec_panel fill:#1168bd,color:#fff,stroke:#1168bd
    style exec_node fill:#1168bd,color:#fff,stroke:#1168bd
    style start_exec fill:#438dd5,color:#fff,stroke:#438dd5
    style cli_path fill:#438dd5,color:#fff,stroke:#438dd5
    style runner fill:#438dd5,color:#fff,stroke:#438dd5
    style stream fill:#438dd5,color:#fff,stroke:#438dd5
    style publish fill:#438dd5,color:#fff,stroke:#438dd5
    style replay fill:#438dd5,color:#fff,stroke:#438dd5
    style cli fill:#999,color:#fff,stroke:#999
    style registry fill:#999,color:#fff,stroke:#999
    style browser fill:none,stroke:#1168bd,stroke-dasharray:5 5
    style server fill:none,stroke:#438dd5,stroke-dasharray:5 5
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

## Sequence Diagram

```mermaid
sequenceDiagram
    actor User
    participant AB as ApprovalBar
    participant SE as start_execution
    participant CLI as PAS CLI
    participant FS as Filesystem
    participant PR as PipelineRunner
    participant BC as Broadcast
    participant SSE as stream_events
    participant EP as ExecutionPanel

    User->>AB: Click Approve & Execute
    AB->>SE: POST server fn

    SE->>FS: Check spec.md exists
    SE->>CLI: pas decompose spec.md
    CLI-->>SE: Epic ID: attractor-xxx

    SE->>CLI: pas scaffold attractor-xxx
    CLI-->>FS: Write .dot file
    CLI-->>SE: exit 0

    SE->>FS: Read .dot file
    SE->>SE: Parse → PipelineGraph
    SE->>PR: tokio::spawn
    SE-->>AB: { session_id, epic_id }

    AB->>EP: on_approve(session_id)
    EP->>SSE: EventSource connect

    loop Each pipeline node
        PR->>BC: node_start
        BC-->>SSE: broadcast
        SSE-->>EP: SSE event
        EP->>EP: Add node (InProgress)

        PR->>PR: Execute handler

        PR->>BC: node_complete
        BC-->>SSE: broadcast
        SSE-->>EP: SSE event
        EP->>EP: Update status + cost
    end

    PR->>BC: pipeline_complete
    BC-->>SSE: broadcast
    SSE-->>EP: SSE event
    EP->>EP: Show Done badge
    PR->>PR: clear_session_state
```
