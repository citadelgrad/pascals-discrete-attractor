# PAS (Pascal's Discrete Attractor) - Agent Instructions

## Build & Test

```bash
cargo build --release          # Build CLI binary
cargo test                     # Run all tests
cargo test -p attractor-dot    # Test a single crate
cargo clippy --workspace       # Lint
cargo fmt --all -- --check     # Format check
```

The CLI binary is `pas`. Install with `./install.sh` or `cargo install --path crates/attractor-cli`.

## Versioning

All crates share a single version in workspace root `Cargo.toml` under `[workspace.package]`. Each crate inherits via `version.workspace = true`. **Never set versions directly in individual crates.** Bump only in the workspace root, then run `cargo check`.

## Key Gotchas

- The default `codergen` handler shells out to the local `claude` CLI — it requires Claude Code installed, no API key needed
- Direct LLM handlers (OpenAI/Anthropic/Gemini) need their respective `*_API_KEY` env vars
- Pipeline files use a strict DOT subset — see `docs/dot-dialect.md` for the grammar, supported features, and what breaks the parser. Read this before generating or editing `.dot` files.
- Integration tests are in `crates/attractor-pipeline/tests/integration.rs`

## Docs Reference

| Doc | Contents |
|-----|----------|
| `docs/dot-dialect.md` | **Attractor DOT dialect** — grammar, value types, supported/unsupported features, pipeline semantics |
| `docs/guide.md` | Pipeline patterns, planning workflow, handler dispatch |
| `docs/cli-reference.md` | CLI commands, flags, environment setup |
| `docs/task-verification.md` | Handler dispatch, goal gates, edge routing, budget guards |

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:7510c1e2 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
