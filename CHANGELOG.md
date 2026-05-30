# Changelog

All notable changes to PAS are documented here.

## [0.7.0] — 2026-05-29

### Added

- **`pas launch` command** — end-to-end workflow: discovers `*-spec.md` + `*-prd.md` pairs in a directory, generates `.dot` pipelines, validates them, and runs them sequentially. Use zero-padded prefixes (`phase-01-spec.md`) to control execution order.
- **Directory mode for `pas run`** — pass a directory instead of a `.dot` file to run all `*.dot` files in that directory sequentially in lexical order.
- **`--fresh` flag for `pas run`** — discard saved checkpoints and start the pipeline from scratch.

### Fixed

- Correct truncation arithmetic and documentation for multibyte strings in context accumulation.
- Missing `tool_name` argument in `tool_result` call in Anthropic provider tests.
- `ToolResult` call sites updated for `tool_name` field added in the types crate.
- Resolved all clippy and build errors introduced during the lint-fix refactoring pass.
- 13 correctness and robustness bugs fixed across all 8 crates (agent, LLM, pipeline, tools, dot, CLI, types, web) via a systematic codebase audit.
- Additional correctness fixes across agent, LLM, and pipeline crates.
- Corrupted `phase-1-spec.dot` restored with valid DOT syntax.

### Changed

- `decompose --validate` flag now accepts an explicit epic ID argument for coverage checking.
- Dependency bump: `rand` 0.8.5 → 0.8.6, `rustls-webpki` 0.103.10 → 0.103.13.
- Security: `rand` 0.9.2 → 0.9.3 (fixes soundness issue with custom loggers and `rand::rng()`).

### Documentation

- Added DOT dialect reference (`docs/dot-dialect.md`).
- Added Reckoner software factory feature plan.
- Added reference to [strongdm/attractor](https://github.com/strongdm/attractor).
- `docs/cli-reference.md` updated: `launch` command, `run` directory mode, `--fresh` flag.

## [0.6.0] — 2026-05-15

Initial public release with DOT-based pipeline runner, Claude Code integration, multi-provider LLM support, goal gates, checkpoint/resume, and the full planning workflow (PRD → spec → beads → pipeline).
