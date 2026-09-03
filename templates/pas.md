---

## PAS Pipeline System

This project uses **PAS** (Pascal's Discrete Attractor) — a DOT-based pipeline runner that executes multi-step AI workflows. Each pipeline is a `.dot` file whose task nodes explicitly select a Claude Code, Codex, or Gemini CLI provider. Pipelines live in `pipelines/`.

### Running a pipeline

```bash
# Validate before running
pas validate pipelines/my-pipeline.dot

# Run with working directory set to this repo
pas run pipelines/my-pipeline.dot -w .

# Inspect structure
pas info pipelines/my-pipeline.dot
```

### Creating a pipeline

When asked to "build a pipeline" or "use a pipeline for this feature/bug", create a `.dot` file in `pipelines/`. Follow this structure:

```dot
digraph PipelineName {
    label="Human-readable pipeline name"
    goal="Describe the objective. Reference beads issue IDs."
    model="sonnet"

    start [shape="Mdiamond", label="Start"]
    done  [shape="Msquare",  label="Done"]

    // Provider-backed nodes send the prompt to the selected local CLI.
    my_node [
        shape="box"
        label="Short Label"
        llm_provider="claude"
        prompt="Detailed instructions for what the selected provider should do in this step."
    ]

    start -> my_node -> done
}
```

### Node attributes

| Attribute | Purpose | Example |
|-----------|---------|---------|
| `prompt` | The task sent to the selected provider CLI | `prompt="Fix the bug in auth.py"` |
| `llm_provider` | Required for provider-backed nodes: `claude`, `codex`, or `gemini` | `llm_provider="claude"` |
| `llm_model` | Override model per node | `llm_model="haiku"` |
| `allowed_tools` | Restrict tools (read-only, git-only, etc.) | `allowed_tools="Read,Grep,Glob"` |
| `max_budget_usd` | Spending cap for this node | `max_budget_usd="1.00"` |
| `goal_gate` | Node must succeed for pipeline to complete | `goal_gate=true` |
| `retry_target` | Where to loop back on goal gate failure | `retry_target="implement"` |

### Node shapes

| Shape | Meaning |
|-------|---------|
| `Mdiamond` | Start node (exactly one) |
| `Msquare` | Exit node (exactly one) |
| `box` | Standard task — runs the selected provider CLI with the prompt |
| `diamond` | Conditional; with a prompt or explicit `type="codergen"` it runs the selected provider, otherwise it routes by existing context |
| `hexagon` | Human gate — pauses for human approval/input |
| `parallelogram` | Tool node — runs a shell command via `tool_command` |

### Edge routing (conditional nodes)

For prompted diamond-shaped nodes, give outgoing edges `label` and `condition`
attributes. The selected provider is asked to output one of the labels, which
routes the pipeline. By default, a diamond without a prompt is pass-through
routing. Explicit `type="codergen"` is the exception: it runs the selected
provider even without a prompt and requires `llm_provider`.

```dot
verify [shape="diamond", label="Check Quality", node_type="conditional",
        llm_provider="claude",
        prompt="Review the changes. Respond PASS or FAIL on the last line."]

verify -> next_step [label="PASS", condition="preferred_label=PASS"]
verify -> fix_step  [label="FAIL", condition="preferred_label=FAIL"]
```

### Standard pipeline pattern for features/bugs

Follow this 6-phase pattern when building pipelines for beads issues:

```
start → investigate → implement → write_tests → run_tests → verify
                                                                ├─ PASS → close_issue → done
                                                                └─ FAIL → fixup ──→ verify (loop)
```

1. **investigate** — Read-only. `allowed_tools="Read,Grep,Glob"`. Understand the code before touching it. Write findings to `.pas/`.
2. **implement** — Make the code changes. Keep the prompt specific with file paths and exact requirements.
3. **write_tests** — Write tests matching existing project patterns. Reference specific test directories.
4. **run_tests** — Execute tests. Prompt should include the exact test command (e.g. `uv run pytest tests/ -k sync`).
5. **verify** — Conditional diamond. Run linter + review changes. Output PASS or FAIL.
6. **fixup** — Only reached on FAIL. Fix issues and loop back to verify.
7. **close_issue** — Commit, close beads issue, sync. Use `allowed_tools="Bash(bd:*),Bash(git:*)"`.

### Key rules for prompts

- **Be specific.** Include file paths, function names, and exact commands. Provider CLI subprocesses have no prior pipeline-step context beyond the prompt and accumulated PAS context.
- **One concern per node.** Don't combine investigation and implementation.
- **Write output files.** If a node generates a report or analysis, tell it to write to `.pas/filename.md`.
- **Reference the beads issue** in the `goal=` attribute so every node has context.
- **Use read-only tools for investigation.** `allowed_tools="Read,Grep,Glob"` prevents premature edits.
- **Include test commands.** Don't say "run tests" — say `cd mlb_fantasy_jobs && uv run pytest tests/ -x -v -k sync_player`.

### Planning workflow (PRD → Spec → Beads → Pipeline)

PAS includes commands to go from requirements to running pipeline:

```bash
# Generate a PRD or spec from a description
pas plan --prd --from-prompt "Add feature X"
pas plan --spec --from-prompt "Add feature X"

# Or copy blank templates for manual editing
pas plan --prd
pas plan --spec

# Generate a pipeline directly from spec (no beads required)
pas generate spec.md
pas generate prd.md spec.md

# Or decompose spec into beads epic + tasks
pas decompose .pas/spec.md

# Scaffold pipeline from the beads epic
pas scaffold <EPIC_ID>

# Run it
pas run pipelines/<EPIC_ID>.dot -w .
```

There's also a meta-pipeline that chains the full workflow with human review gates:

```bash
pas run templates/plan-to-execute.dot -w .
```

### Example

See `pipelines/fix-sync-partial-failure.dot` for a complete pipeline that fixes beads issue `baseball-v3-vfd5`.

---
