# Changelog

All notable changes to PAS are documented here.

## [0.9.3] — 2026-08-09

### Fixed

- `pas run` now prints the true total cost across looping pipelines. Previously the final "Total cost" line was computed by re-summing `<node>.cost_usd` keys out of the pipeline's final context, a map that only keeps the last write per key — so a node re-run via a `loop_restart` edge (e.g. a quality fix/verify retry loop) silently dropped every earlier iteration's cost from the total. `PipelineResult` now carries a `total_cost` field populated from the engine's per-execution accumulator, which is unaffected by `loop_restart`, and both the CLI and the web streaming runner report from it.
- Added a preflight warning (`PROVIDER_COST_UNTRACKED`) for nodes using the `codex` or `gemini` CLI providers: neither reports a per-call dollar cost, so such nodes always count as $0 toward `Total cost` and `--max-budget-usd` even though real spend occurred.

## [0.9.2] — 2026-08-08

### Fixed

- `pas scaffold`'s `epic-runner.dot` template now sets an explicit `timeout` on every node (120s for routing/conditional nodes, 300s for medium tasks, 900s for heavy implementation/test nodes). Newly scaffolded epic-runner pipelines no longer silently fall back to the hardcoded 600s kill timeout on long-running `codergen` steps.

### Changed

- `pas run`'s checkpoint-resume message is now a highlighted banner. It shows the node being resumed into, steps run so far, cost spent so far, and how long ago the checkpoint was saved — colored when the terminal supports it, plain text otherwise. It deliberately omits a "completed N of M" count: looping pipelines (e.g. the epic-runner template) clear their completed-node history on every loop restart, so that count would be misleading.

## [0.9.1] — 2026-08-08

### Fixed

- `pas run` now actually invokes preflight checks. The `CODERGEN_NO_TIMEOUT` warning (added in 0.9.0) was built and tested but never called from any CLI command, so it never fired. It now runs at the start of every `pas run`, before the pipeline starts.

## [0.9.0] — 2026-07-18

### Added

- Added subscription-friendly Claude Code isolation for `codergen`, with manifest and CLI controls for PAS-owned settings, agents, tools, and plugin directories.
- Added optional strict `claude --bare` mode for API-key-only isolation.
- Added preflight warnings for `codergen` nodes without a resolved timeout.

### Changed

- Documented the new Claude isolation controls in the CLI reference and guide.
- Removed the experimental cross-run `codergen` response cache before release.

## [0.8.0] — 2026-06-27

### Tests

- Added 6 boundary-condition tests to `attractor-pipeline` closing mutation testing gaps (score: 67 % → ~85 %).
  - `step_limit_exact_boundary_does_not_abort` — step cap uses strict `>`, not `>=`
  - `budget_limit_exact_equality_does_not_abort` — cost equal to budget should not abort
  - `quality_loop_fires_at_iteration_beyond_max_fix_iterations` — loop counter fires at N+1 entries, not N; handler call count asserted
  - `quality_retry_warning_injected_on_second_iteration` — `__quality_retry_warning` context key verified at iteration 2
  - `fail_handler_with_no_outgoing_edge_returns_handler_error` — Fail on a dead-end node returns `HandlerError`, not silent success
  - `truncate_head_tail_exact_boundary_not_truncated` — exactly `head+tail` lines returns unchanged text

## [0.7.2] — 2026-06-20

### Added

- **`QualityHandler`** — manifest-driven quality gate with env isolation, per-stage telemetry, head/tail output truncation, and failure footprint tracking (`attractor-quality`, `attractor-pipeline`).
- **`pas.toml` manifest** — walk-up resolution from working directory; `[quality.stages]` drives quality checks without node attributes.
- **Quality loop control** — engine enforces `max_fix_iterations`, injects `__quality_retry_warning` context on retry, checkpoints with `schema_version`.
- **Preflight check** — warns when a quality node is present but no `pas.toml` manifest is found.
- **Trust store** — `attractor-quality` records trust decisions; LLM enrichment stub and `pas trust` CLI commands.
- **`pas init`** — toolchain detection, template emission, TUI confirmation dialog, `--yes` non-interactive mode.

### Fixed

- Skip `.dot` regeneration on checkpoint resume to protect already-valid pipeline files.
- Hardened quality loop edge cases: footprint extraction, loop-counter key scoping, and cooldown sleep placement.
- 7 correctness bugs found and fixed via fresh-eyes audit of the quality loop and engine flow.

### Changed

- `QualityHandler` exported from `attractor_pipeline` public API.
- Code review refactoring pass for the attractor-5b8 feature branch.

### Documentation

- `docs/cli-reference.md` updated with `pas init` and quality handler node attributes.
- Quality handler design spec and integration tests added.

## [0.7.1] — 2026-05-29

### Fixed

- Replace `.expect()` panics in `init_db()` with proper error propagation via `sqlx::Error::Configuration` and `sqlx::Error::Io`.

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
