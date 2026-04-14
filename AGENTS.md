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
