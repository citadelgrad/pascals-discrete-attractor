# CLI Reference

## Synopsis

```
pas [OPTIONS] <COMMAND>
```

## Global Options

| Option | Short | Description |
|--------|-------|-------------|
| `--verbose` | `-v` | Enable debug-level logging. Shows detailed handler execution, edge selection decisions, and context updates. |

---

## Commands

### `run` — Execute a pipeline

Parses the DOT file, compiles and validates canonical semantics, then executes the
pipeline. Each provider-backed `codergen` node starts its explicitly selected
local Claude Code, Codex, or Gemini CLI.

```
pas run <PIPELINE> [OPTIONS]
```

#### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `PIPELINE` | Yes | Path to the `.dot` pipeline file |

#### Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--workdir <DIR>` | `-w` | current directory | Working directory for provider CLI sessions, so file paths in prompts are relative to it. |
| `--logs <DIR>` | `-l` | `.pas/logs` | Directory for log output. |
| `--dry-run` | — | false | Parse and validate the pipeline without executing provider processes or tools; no cost is incurred. |
| `--max-budget-usd <AMOUNT>` | — | $200 | Maximum tracked spend across all nodes. Pipeline aborts with an error if exceeded. Codex and Gemini CLI calls report no dollar cost and therefore do not count toward this limit; use `--max-steps` to bound them. |
| `--max-steps <COUNT>` | — | 200 | Maximum number of node executions before aborting. Prevents runaway loops. A 6-node pipeline that loops 3 times = 18 steps. |
| `--fresh` | — | false | Discard any saved checkpoint and start from the beginning. By default, re-running the same command resumes from the last completed node. |
| `--codergen-claude-settings-mode <MODE>` | — | `subscription-bare` | Claude settings mode for `codergen` nodes: `subscription-bare`, `strict-bare`, or `inherit`. |
| `--codergen-claude-setting-sources <LIST>` | — | — | Comma-separated Claude setting sources for `inherit` mode (`user,project,local`). Required when inheriting. |
| `--codergen-claude-settings <JSON_OR_FILE>` | — | — | PAS-owned Claude settings JSON or file path for `codergen` nodes. Does not imply user settings inheritance. |
| `--codergen-claude-tools <TOOLS>` | — | Claude default | Explicit Claude built-in tool surface for `codergen` nodes, e.g. `Read,Edit` or `""` to disable built-ins. |
| `--codergen-claude-agents <JSON>` | — | — | PAS-owned Claude agents JSON for `codergen` nodes. |
| `--codergen-claude-plugin-dir <DIR>` | — | — | PAS-owned Claude plugin directory for `codergen` nodes. Repeatable. |
| `--codergen-claude-mcp-config <JSON_OR_FILE>` | — | none | Explicit MCP config for `codergen` nodes. `--strict-mcp-config` remains enabled. |

PAS resolves each run-control field independently using `caller > manifest > permitted graph defaults > built-ins`.
Graph defaults are permitted only for workflow semantics and the quality node's
`max_fix_iterations`; graphs cannot author global `dry_run`, `workdir`,
`max_steps`, `max_budget_usd`, `quality_disabled`,
`quality_max_fix_iterations`, or `codergen.claude.*` controls. CLI/API values
remain authoritative. Invalid typed controls and reserved graph attributes fail
before provider/tool startup, log creation, or before `--fresh` deletes a checkpoint.

The built-ins are `dry_run=false`, `max_steps=200`, a $200 tracked global
budget, the canonical current directory, quality enabled, three quality-fix
iterations, and Claude `subscription_bare` isolation. A node-level
`max_budget_usd` remains a separate per-Claude-session cap.

#### Directory mode

When `PIPELINE` is a directory, `run` collects all `*.dot` files and executes them sequentially in lexical order. Use zero-padded names to control execution order (`phase-01.dot`, `phase-02.dot`, `phase-11.dot`). Checkpoints apply per-pipeline within the run.

#### Output

Prints:
- Pipeline name and goal
- Working directory (if set)
- Per-node log lines with node ID, label, turns, cost, and error status
- List of completed nodes
- Total cost across all nodes

#### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Pipeline completed successfully |
| 1 | Pipeline failed (validation error, handler error, goal gate unsatisfied, or quality loop exhausted) |
| 2 | `pas.toml` found but not trusted — run `pas trust add` or set `PAS_TRUST_THIS=1` |

#### Quality manifest warnings

If a pipeline contains a `quality` node but no `pas.toml` is found in the working directory tree, `pas run` emits a `[WARN]` preflight diagnostic and continues. Stages will use the node's `quality_checks` attribute as a fallback; the manifest-driven stage list (`[quality.stages]`) is unavailable.

To suppress the warning, run `pas init` in your project root to generate a `pas.toml`.

#### Claude settings isolation for `codergen`

Claude-backed `codergen` nodes run in PAS-controlled isolation by default. PAS passes Claude Code `--safe-mode`, `--strict-mcp-config`, and `--disable-slash-commands` so subscription auth still works while personal hooks, skills, plugins, MCP servers, and other ambient Claude Code customizations are suppressed as much as Claude allows without literal bare mode.

Modes:

| Mode | Claude behavior | Auth impact |
|------|-----------------|-------------|
| `subscription-bare` | Default. Uses `--safe-mode` plus PAS-owned explicit settings/tools/agents/plugins/MCP when configured. | Works with normal Claude subscription auth. |
| `strict-bare` | Uses Claude's literal `--bare`. Strongest isolation. | Requires API-key/auth-helper-compatible Claude auth; normal subscription OAuth/keychain auth is not read. |
| `inherit` | Does not pass `--safe-mode` or `--bare`; loads explicit `--setting-sources`. | Personal hooks/settings may run. Opt in loudly. |

Equivalent `pas.toml` config:

```toml
[codergen.claude]
settings_mode = "subscription_bare" # subscription_bare | strict_bare | inherit
setting_sources = ["user"]           # only for inherit
settings_json = "{\"enabledPlugins\":{}}"
tools = "Read,Edit"
agents_json = "{}"
plugin_dirs = [".pas/claude-plugin"]
mcp_config_json = "{}"
```

CLI flags override the corresponding `pas.toml` field for the current run;
unset CLI fields continue to inherit their individual manifest values.

---

### `validate` — Check a pipeline for errors

Runs canonical semantic compilation followed by nine structural checks without
executing the pipeline. Useful for checking typed roles, providers, syntax, and
structure before committing a DOT file.

```
pas validate <PIPELINE>
```

#### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `PIPELINE` | Yes | Path to the `.dot` pipeline file |

#### Output

If valid:
```
Pipeline is valid
```

If issues found:
```
[ERROR] provider_valid (node: analyze): Node 'analyze' has unknown llm_provider 'llama'
  Fix: Use claude/anthropic, codex/openai, or gemini/google
[WARN] prompt_on_llm_nodes (node: review): Node 'review' (handler=codergen) has no prompt and label matches id
  Fix: Add a prompt or a descriptive label attribute
```

#### Exit codes

| Code | Meaning |
|------|---------|
| 0 | No errors (warnings are OK) |
| 1 | One or more errors found |

---

### `info` — Inspect a pipeline

Displays the pipeline structure: name, goal, node count, edge count, start/exit nodes, and a list of all nodes with their resolved kind, handler, and provider.

```
pas info <PIPELINE>
```

#### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `PIPELINE` | Yes | Path to the `.dot` pipeline file |

#### Output

```
Pipeline: FixSyncPartialFailure
Goal: Fix baseball-v3-vfd5: sync_player_data silently returns partial results
Nodes: 9
Edges: 9
Start: start (Start)
Exit: done (Done)

Nodes:
  done [Done] kind=Exit handler=exit provider=-
  implement [Implement Fix] kind=Task handler=codergen provider=codex
  start [Start] kind=Start handler=start provider=-
  verify [Verify Quality] kind=Conditional { llm_backed: true } handler=codergen provider=claude
  ...
```

---

### `plan` — Generate PRD or spec documents

Creates a PRD (product requirements document) or technical specification from a template. Optionally uses Claude to generate content from a one-line description.

```
pas plan [OPTIONS]
```

#### Options

| Option | Required | Default | Description |
|--------|----------|---------|-------------|
| `--prd` | One of `--prd`/`--spec` | — | Generate a PRD document |
| `--spec` | One of `--prd`/`--spec` | — | Generate a technical specification |
| `--from-prompt <DESC>` | No | — | Use Claude to generate the document from this description instead of copying the blank template |
| `--output <PATH>` | No | `.pas/prd.md` or `.pas/spec.md` | Output file path |

#### Output

Copies the template or generates content and writes to the output path. Prints next steps for manual editing or beads integration.

#### Examples

```bash
# Copy blank PRD template for manual editing
pas plan --prd

# Generate a PRD from a description
pas plan --prd --from-prompt "Add OAuth2 authentication with Google and GitHub providers"

# Generate a spec to a custom path
pas plan --spec --output docs/specs/auth-spec.md
```

---

### `decompose` — Convert spec to beads issues

Reads a technical specification file and uses Claude to generate beads CLI commands that create an epic, child tasks, and dependencies.

```
pas decompose <SPEC_PATH> [OPTIONS]
```

#### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `SPEC_PATH` | Yes | Path to the spec markdown file |

#### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--dry-run` | false | Print the generated `bd` commands without executing them |

#### Output

Creates a beads epic with child tasks and dependencies. Prints the epic ID, task count, and dependency count. On `--dry-run`, prints the shell script that would be executed.

#### Examples

```bash
# Preview what would be created
pas decompose .pas/spec.md --dry-run

# Create the epic and tasks
pas decompose .pas/spec.md

# Decompose a spec from a custom path
pas decompose docs/specs/auth-spec.md
```

---

### `generate` — Generate pipeline from spec files

Uses Claude to convert a technical specification (and optional PRD) into a pipeline `.dot` file. Supports single-file and directory modes.

```
pas generate [OPTIONS] <FILE>...
pas generate <DIRECTORY>
```

#### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `FILE` | Yes | Spec file path, or PRD then spec (positional), or a directory of `*-spec.md` files |

#### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--prd <PATH>` | — | Explicit PRD file path |
| `--spec <PATH>` | — | Explicit spec file path |
| `--output <PATH>` | `pipelines/<stem>.dot` | Output file path |

#### Modes

**Single-file mode:**
```bash
pas generate my-spec.md                    # Spec only
pas generate my-prd.md my-spec.md          # PRD + spec (positional)
pas generate --prd prd.md --spec spec.md   # PRD + spec (named)
```

**Directory mode:**
```bash
pas generate docs/implementation/
```

In directory mode, files ending in `-spec.md` are discovered and sorted lexically. Each spec is paired with a matching `-prd.md` if one exists (e.g. `auth-spec.md` pairs with `auth-prd.md`). One `.dot` pipeline is generated per spec.

#### Timeout tiers

Generated pipelines assign timeouts to every node based on complexity:

| Tier | Timeout | Used for |
|------|---------|----------|
| Trivial | 120s | Conditionals, haiku routing, reading a single file |
| Light | 300s | Linting, formatting checks, simple single-step verification |
| Standard | 600s | Investigation, verification with iteration, fixups, most work nodes |
| Heavy | 900s | Implementing features, writing substantial new code |
| Intensive | 1200s | Full test suites, large refactors, multi-step builds |

#### Output

Writes the pipeline to the output path, validates it, and prints node count and validation status.

#### Examples

```bash
# Generate from a spec
pas generate docs/auth-spec.md

# Generate with PRD for richer context
pas generate docs/auth-prd.md docs/auth-spec.md

# Generate all pipelines from a directory of specs
pas generate docs/implementation/

# Then run it
pas run pipelines/auth-spec.dot -w .
```

---

### `scaffold` — Generate pipeline from beads epic

Creates a pipeline DOT file from a beads epic. The pipeline iterates through all child tasks of the epic, implementing each one.

```
pas scaffold <EPIC_ID> [OPTIONS]
```

#### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `EPIC_ID` | Yes | Beads epic ID (e.g., `beads-asr`) |

#### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--output <PATH>` | `pipelines/<EPIC_ID>.dot` | Output file path |

#### Output

Generates a DOT pipeline file from the `epic-runner` template with the epic ID substituted. Validates the result and prints node count and validation status.

#### Examples

```bash
# Scaffold a pipeline for an epic
pas scaffold attractor-asr

# Scaffold to a custom path
pas scaffold attractor-asr --output pipelines/auth-feature.dot

# Then run it
pas run pipelines/attractor-asr.dot -w .
```

---

### `launch` — Generate, validate, and run end-to-end

Takes a directory of spec files, generates `.dot` pipelines from them, validates all of them, then runs them sequentially. Equivalent to running `generate` → `validate` → `run` on each pipeline in order.

```
pas launch <DOCS_DIR> [OPTIONS]
```

#### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `DOCS_DIR` | Yes | Directory containing `*-spec.md` (required) and `*-prd.md` (optional) files |

#### Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--workdir <DIR>` | `-w` | current directory | Working directory for selected provider CLI sessions during the run phase. |
| `--output <DIR>` | `-o` | `pipelines/` | Directory where generated `.dot` files are written. |
| `--dry-run` | — | false | Generate and validate pipelines but don't execute them. |
| `--max-budget-usd <AMOUNT>` | — | $200 | Maximum tracked spend across all nodes in each pipeline. Codex and Gemini CLI calls do not report dollar cost and therefore do not count toward this limit. |
| `--max-steps <COUNT>` | — | 200 | Maximum node executions per pipeline. |
| `--fresh` | — | false | Ignore checkpoints and start each pipeline from scratch. |
| `--codergen-claude-settings-mode <MODE>` | — | `subscription-bare` | Claude settings mode for generated pipelines' `codergen` nodes. Same semantics as `pas run`. |
| `--codergen-claude-setting-sources <LIST>` | — | — | Comma-separated setting sources for `inherit` mode. |
| `--codergen-claude-settings <JSON_OR_FILE>` | — | — | PAS-owned Claude settings JSON or file path for run-phase `codergen` nodes. |
| `--codergen-claude-tools <TOOLS>` | — | Claude default | Explicit Claude built-in tool surface for run-phase `codergen` nodes. |
| `--codergen-claude-agents <JSON>` | — | — | PAS-owned Claude agents JSON for run-phase `codergen` nodes. |
| `--codergen-claude-plugin-dir <DIR>` | — | — | PAS-owned Claude plugin directory for run-phase `codergen` nodes. Repeatable. |
| `--codergen-claude-mcp-config <JSON_OR_FILE>` | — | none | Explicit MCP config for run-phase `codergen` nodes. |

#### How it works

1. **Generate** — discovers `*-spec.md` files in `DOCS_DIR`, pairs each with a `*-prd.md` if one exists (matched by replacing `-spec` with `-prd`), and generates one `.dot` pipeline per spec. Files are sorted lexically — use zero-padded prefixes to control order (`phase-01-spec.md`, `phase-02-spec.md`).
2. **Validate** — runs all lint rules against every generated pipeline. Stops if any pipeline has errors.
3. **Run** — executes each validated pipeline sequentially with checkpoint/resume.

#### Exit codes

| Code | Meaning |
|------|---------|
| 0 | All pipelines completed successfully |
| 1 | Generation failed, validation error, or pipeline execution failed |

#### Examples

```bash
# Generate + validate + run all specs in docs/implementation/
pas launch docs/implementation/ -w .

# With a budget cap (recommended for unattended runs)
pas launch docs/implementation/ -w . --max-budget-usd 30.00

# Dry run to preview what would be generated and validated
pas launch docs/implementation/ --dry-run

# Write generated .dot files to a custom directory
pas launch docs/implementation/ -w . -o build/pipelines/
```

---

### `init` — Initialise a `pas.toml` in your project

Detects the project toolchain from well-known config files (`Cargo.toml`, `pyproject.toml`, `package.json`, …) and writes a starter `pas.toml` to the nearest `.git` root.

```
pas init [OPTIONS]
```

#### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--workdir <DIR>` | `.` (current directory) | Directory to inspect for toolchain detection and to write `pas.toml`. |
| `--force` | false | Overwrite an existing `pas.toml` and proceed even without a `.git` root. |
| `--non-interactive` | false | Never prompt; fail instead of asking questions. Suitable for CI. |
| `--no-enrich` | false | Skip LLM enrichment for polyglot repos (always skipped in v1; reserved for future use). |
| `--dry-run` | false | Print what would be written without touching the filesystem. |

#### How it works

1. Walks up from `--workdir` to find the nearest `.git` root.
2. Detects the primary toolchain by looking for `Cargo.toml`, `pyproject.toml`, `package.json`, `go.mod`, etc.
3. In interactive mode, shows a preview of the generated `pas.toml` and asks for confirmation.
4. Writes `pas.toml` with `[project]`, `[toolchain]`, and `[quality]` sections pre-populated for the detected language.

#### Generated file layout

```toml
[project]
name = "my-project"
version = "0.1.0"

[toolchain]
language = "rust"
version = "1.80"

[quality]
stages = ["fmt", "lint", "test"]
max_fix_iterations = 3

[quality.hooks.fmt]
cmd = "cargo fmt --check"

[quality.hooks.lint]
cmd = "cargo clippy -- -D warnings"

[quality.hooks.test]
cmd = "cargo test"
```

#### Exit codes

| Code | Meaning |
|------|---------|
| 0 | `pas.toml` written successfully (or `--dry-run` printed it) |
| 1 | Error writing the file |
| 4 | No `.git` root found and `--force` not passed in non-interactive mode |

#### Examples

```bash
# Detect toolchain and write pas.toml interactively
pas init

# Dry-run: preview the generated file without writing it
pas init --dry-run

# CI-safe: non-interactive, fail if no .git root
pas init --non-interactive

# Force-write even without a .git root (e.g. in a monorepo subdirectory)
pas init --force

# Init for a project in a different directory
pas init --workdir ~/projects/my-app
```

---

### `trust` — Manage the pas.toml trust store

Controls which `pas.toml` manifests are trusted to run quality stages. The trust store lives at `$XDG_CONFIG_HOME/pas/trusted.json` (default: `~/.config/pas/trusted.json`).

```
pas trust <ACTION>
```

#### Subcommands

| Action | Description |
|--------|-------------|
| `add <PATH> <HASH>` | Add a manifest to the trust store by path + blake3 hash |
| `remove <PATH> <HASH>` | Remove a manifest from the trust store |
| `list` | Print all currently trusted manifests |

#### Trust bypass environment variables

| Variable | Value | Effect |
|----------|-------|--------|
| `PAS_TRUST_THIS` | `1` | Trust all manifests unconditionally (for development) |
| `PAS_AGENT` | `1` | Non-interactive agent mode — never prompts, never trusts |
| `PAS_NON_INTERACTIVE` | `1` | Suppress trust prompts in CI |

#### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Action completed successfully |
| 1 | Trust store corrupted or I/O error |
| 2 | Manifest not trusted (checked at `pas run` time, not here) |
| 3 | Trust store corrupted (deserialization failure) |

#### Examples

```bash
# Add a manifest to the trust store
pas trust add /path/to/project/pas.toml <blake3-hash>

# List all trusted manifests
pas trust list

# Remove a manifest
pas trust remove /path/to/project/pas.toml <blake3-hash>
```

---

## Global exit code reference

| Code | Meaning | Raised by |
|------|---------|-----------|
| 0 | Success | all commands |
| 1 | General failure (validation error, handler error, quality loop exhausted) | `run`, `validate`, `launch`, `trust` |
| 2 | Manifest not trusted | `run` (when quality stages attempted with untrusted `pas.toml`) |
| 3 | Trust store corrupted | `run`, `trust` |
| 4 | No `.git` root found without `--force` | `init` |

---

## Examples

### Run with a budget limit (recommended for loops)

```bash
pas run pipelines/epic-runner.dot -w . --max-budget-usd 10.00
```

If total spend across all nodes exceeds $10, the pipeline stops with an error. Prevents a looping pipeline from running up a massive bill overnight.

### Run with a step limit

```bash
pas run pipelines/epic-runner.dot -w . --max-steps 50
```

Limits the pipeline to 50 node executions. For an epic runner with ~7 nodes per loop, this allows ~7 iterations before stopping. The default is 200 steps.

### Run with both limits (safest for unattended runs)

```bash
pas run pipelines/epic-runner.dot -w . --max-budget-usd 20.00 --max-steps 100
```

The pipeline stops at whichever limit is hit first.

### Run a pipeline in your project directory

```bash
pas run pipelines/fix-bug.dot -w .
```

The `-w .` sets the working directory to the current directory. Selected provider CLIs and tool commands use this path as their working directory.

### Run a pipeline for a different project

```bash
pas run ~/pipelines/deploy-check.dot -w ~/projects/my-app
```

The pipeline file and working directory don't need to be in the same place.

### Validate before running

```bash
pas validate pipelines/new-feature.dot && \
pas run pipelines/new-feature.dot -w .
```

Only runs if validation passes.

### Inspect a pipeline to see its structure

```bash
pas info pipelines/epic-runner.dot
```

Quick way to see the nodes and verify the graph shape before running.

### Debug a failing pipeline

```bash
pas -v run pipelines/fix-bug.dot -w .
```

The `-v` flag enables debug logging. You'll see:
- Which handler is selected for each node
- Edge selection decisions (condition evaluation, label matching)
- Context updates after each node
- Goal gate check results

### Dry run to verify parsing

```bash
pas run pipelines/complex-feature.dot --dry-run
```

Parses and validates the pipeline, prints the structure, but doesn't execute any selected provider CLI or tool command. Zero cost.

### Run from anywhere with an alias

Add to your shell profile (`~/.zshrc` or `~/.bashrc`):

```bash
alias pas='~/.local/bin/pas'
```

Then:

```bash
cd ~/projects/my-app
pas run pipelines/fix-auth.dot -w .
pas validate pipelines/new-feature.dot
pas info pipelines/deploy.dot
```

### Pipeline for a beads issue

```bash
# Look up the issue
bd show baseball-v3-vfd5

# Run the pipeline that fixes it
pas run pipelines/fix-sync-partial-failure.dot -w ~/gt/baseball
```

### Process an entire epic

```bash
# Copy the epic runner template
cp /Volumes/qwiizlab/projects/connect-the-bots/docs/examples/epic-runner.dot pipelines/run-epic.dot

# Replace EPIC_ID with your epic
sed -i '' 's/EPIC_ID/baseball-v3-8xey/g' pipelines/run-epic.dot

# Run it — loops through all child tasks
pas run pipelines/run-epic.dot -w .
```

### Chain validate + run in CI or scripts

```bash
#!/bin/bash
set -e

PIPELINE="$1"
WORKDIR="${2:-.}"

echo "Validating $PIPELINE..."
pas validate "$PIPELINE"

echo "Running $PIPELINE in $WORKDIR..."
pas run "$PIPELINE" -w "$WORKDIR"
```

Usage: `./run-pipeline.sh pipelines/fix-bug.dot ~/projects/my-app`

### Full planning workflow (PRD → Spec → Beads → Pipeline → Execute)

```bash
# Step 1: Generate a PRD from a description
pas plan --prd --from-prompt "Add real-time notifications via WebSockets"

# Step 2: Review and edit .pas/prd.md manually

# Step 3: Generate a spec from a description (or copy template and edit)
pas plan --spec --from-prompt "Add real-time notifications via WebSockets"

# Step 4: Review and edit .pas/spec.md manually

# Step 5: Decompose spec into beads epic + tasks
pas decompose .pas/spec.md

# Step 6: Scaffold pipeline from the epic
pas scaffold <EPIC_ID>

# Step 7: Run the pipeline
pas run pipelines/<EPIC_ID>.dot -w .
```

### Run the meta-pipeline (automated full workflow)

```bash
pas run templates/plan-to-execute.dot -w .
```

The meta-pipeline chains all planning steps with human review gates. It generates PRD, pauses for review, generates spec, pauses for review, decomposes into beads, scaffolds the pipeline, validates, and executes.

### Compare two pipelines

```bash
pas info pipelines/v1.dot
pas info pipelines/v2.dot
```

Quick way to compare node counts and structure between pipeline revisions.

---

## Environment

### Required

- Every provider binary selected by an executable `codergen` node must be in
  `PATH`: `claude`, `codex`, and/or `gemini`. Unselected provider binaries are
  not required. Verify a selected binary with `command -v <provider>`.

### Optional

- **`RUST_LOG`** — Override log level (e.g. `RUST_LOG=debug pas run ...`). The `-v` flag sets this to `debug` automatically.

---

## Node-level Claude Code flags

These are set in the `.dot` file as node attributes and passed through to each `claude -p` invocation:

| Node attribute | Claude CLI flag | Effect |
|----------------|----------------|--------|
| `llm_model` | `--model` | Override model for this node |
| `allowed_tools` | `--allowedTools` | Restrict available tools |
| `max_budget_usd` | `--max-budget-usd` | Cap spending for this node |
| Graph `model` | `--model` (fallback) | Default model when node doesn't specify one |

Every node also gets:
- `--output-format json` — for structured output parsing
- `--no-session-persistence` — each node is a fresh session
- `--dangerously-skip-permissions` — allows file edits and bash execution

### Examples in DOT

```dot
// Cheap read-only investigation using haiku
investigate [
    shape="box"
    llm_provider="claude"
    llm_model="haiku"
    allowed_tools="Read,Grep,Glob"
    prompt="Find all usages of deprecated_function"
]

// Expensive deep analysis using opus with a budget cap
analyze [
    shape="box"
    llm_provider="claude"
    llm_model="opus"
    max_budget_usd="5.00"
    prompt="Perform a security audit of the authentication module"
]

// Default model (inherits from graph-level model attribute)
implement [
    shape="box"
    llm_provider="claude"
    prompt="Fix the SQL injection in the search endpoint"
]
```
