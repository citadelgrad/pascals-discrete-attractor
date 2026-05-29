---
title: "feat: pas init + quality handler (SPEC-005 implementation)"
type: feat
status: active
date: 2026-05-29
origin: docs/reviews/SPEC_PAS_INIT-review.md
spec: SPEC_PAS_INIT.md (revised per review before implementation begins)
---

# pas init + `quality` Handler (SPEC-005 Implementation)

## Overview

Implement SPEC-005 (`pas init` subcommand + `pas.toml` manifest + declarative quality loops) after first revising the SPEC to close the four P1 ambiguities surfaced in the review.

This plan covers ten Beads child issues organized into nine implementation phases. The critical path is **SPEC revision → schema + resolution → CLI scaffold → trust model → handler → loop control → validation warning → docs/tests**. Scott's specific concern — *"warn if a repo doesn't have the toml file"* — is Issue #9 (Phase 7) and is wired through the manifest-resolution layer added in Phase 2.

**Locked design decisions** (from planning Q&A on 2026-05-29):

| Decision | Choice | Source |
|---|---|---|
| Tracker | Beads (overrides project memory) | User instruction |
| Schema shape | Explicit `stages = [...]` array + per-stage sub-tables | Q&A response |
| Trust model | direnv-style prompt + auto-trust on `pas init` | Q&A response |
| Handler name | `quality` (matches existing `codergen`/`wait_human`/`parallel` convention) | Q&A response |

---

## Enhancement Summary

**Deepened on:** 2026-05-29
**Sections enhanced:** 8 implementation phases + Dependencies + System-Wide Impact + Risk Analysis
**Research sources used:** security-sentinel, architecture-strategist, performance-oracle, code-simplicity-reviewer, best-practices-researcher, framework-docs-researcher, agent-native-reviewer

### Key Improvements

1. **Security hardening on `cmd` execution.** Per security-sentinel: every spawned process gets `env_clear()` + explicit allowlist (`PATH`, `HOME`, `LANG`, `CARGO_HOME` etc.), canonicalized `cwd` checked against repo root via `Path::starts_with`, optional `shlex` argv-split mode (`cmd_argv = [...]`) to avoid implicit shell, and a denylist scanner on parsed commands flagging `rm -rf`, `sudo`, `curl | sh`, `mkfs`, `dd`, `:(){ :|:& };:` before the trust prompt fires.
2. **Preflight layer instead of `LintRule`.** Per architecture-strategist: Phase 7 introduces a new `pipeline::preflight::run(graph, workdir)` step distinct from `validation` (which stays pure/syntactic). Preflight is where IO-bearing checks live, run after parsing and before execution. Keeps `pas validate` semantics pure-by-default with `--preflight` opt-in.
3. **Streaming output capture + ring buffer.** Per performance-oracle: rather than collecting full stdout/stderr into a `Vec<u8>` and truncating at the end, use a bounded ring buffer fed by `BufReader::lines()` on the child handles. Memory bounded to `truncate_logs_after_bytes` regardless of test verbosity. Head-and-tail truncation (first 25% + last 75%) preserves both setup context and the actual failure.
4. **Process group + `killpg` for orphan-free cancellation.** Per performance-oracle + framework-docs-researcher: spawn each stage in its own process group via `tokio::process::Command::process_group(0)` (stable since tokio 1.40). On timeout, abort, or Ctrl-C, send `SIGTERM` to the *group* via `nix::sys::signal::killpg`, not just the parent pid. `kill_on_drop(true)` is the floor; process groups are the actual fix for `cargo test` spawning grandchildren.
5. **Updated version pins.** Per framework-docs-researcher: `toml = "1"` (1.1.2 — major version bump from 0.8, breaking API surface for `to_string`/`from_str`), `dialoguer = "0.12"`, `directories = "6"` (or migrate to `etcetera` per best-practices-researcher for the better Windows defaults). Use `toml_edit` instead of `toml` for the `pas init` write path so existing hand-edited comments survive a re-run.
6. **`failure_footprint` simplified, but kept.** Per simplicity-reviewer and best-practices-researcher (LangGraph "observation fingerprint" pattern): footprint stays in v1 since loop control needs it, and we standardize on a single 16-byte BLAKE3 hash for both `failure_footprint` and the trust-file content hash. ~10× faster than SHA-256, the truncation doesn't change collision resistance for our window, and committing to one hash crate (`blake3`) rather than two (`sha2` + something else) keeps the dep surface minimal. `blake3` is a genuinely new workspace dep (`cargo metadata` confirms it isn't transitively present today).
7. **`pas trust` as a first-class subcommand.** Per agent-native-reviewer: trust state is agent-relevant (an autonomous loop builder needs to grant trust without a TTY), so trust gets `pas trust [--add|--remove|--list]` as an explicit primitive. The implicit prompt remains for humans; the explicit command serves agents and CI without overloading `pas run --trust`.
8. **Stage-aware `system_guidance`.** Per agent-native-reviewer: telemetry's `system_guidance` field is templated per stage (`format` → "Run `pas init` to regenerate format settings, or fix whitespace", `lint` → "Address the clippy warnings shown above", `test` → "Read the failing test names and fix the underlying logic — do not just delete the tests"). Eliminates the most common LLM failure mode: "fix it" with no domain hint.

### New Considerations Discovered

- **Concurrency on trust-file writes.** Two `pas init` invocations in parallel could clobber `trusted.json`. Use `tempfile::NamedTempFile` + atomic `persist()` rename (performance-oracle finding).
- **`OnceCell` for trust cache.** Re-reading `trusted.json` on every handler invocation across a 10-stage loop is wasteful; cache the parsed set behind `tokio::sync::OnceCell` keyed on the engine lifetime (performance-oracle).
- **Checkpoint `schema_version` field.** Architecture-strategist: add `schema_version: u32` to the checkpoint struct in Phase 6, defaulting to current value via `#[serde(default = "current_version")]`. Lets us evolve the loop-counter shape later without forcing `--fresh` on every user.
- **`--dry-run` everywhere.** Agent-native-reviewer: `pas init --dry-run` should print the planned `pas.toml` to stdout without writing. `pas run --dry-run` should run the preflight + trust check + first stage's command resolution but stop before spawning. Both are agent-native affordances for "what would happen?"
- **Rename `--no-agent` → `--no-enrich`.** Agent-native-reviewer: `--no-agent` reads as "disable agent mode" generally; `--no-enrich` is precise about what it disables (Phase 3c LLM enrichment).
- **AGENTS.md timing.** Agent-native-reviewer: AGENTS.md should be updated *in Phase 5* (when the handler lands) not in Phase 8, so anyone using subagents to build later phases sees the handler contract.

### Alternative v1 Cuts (Considered, Not Adopting Wholesale)

Code-simplicity-reviewer recommended a ~35% LOC reduction by cutting Phase 3c entirely, deferring trust persistence, dropping the footprint hash, and shipping as three PRs. We're keeping the full scope because:
- Phase 3c is gated behind `--no-enrich` and is the smallest crate boundary; deferring it does not simplify the core engine. Retain.
- Deferring trust persistence creates a CVE-shaped gap (`git clone && pas run`). Retain.
- Dropping the footprint creates the runaway-loop bug from review P2-5. Retain (but simplified to BLAKE3 per item 6 above).
- Three-PR shipping cadence: **adopted partially** — Phase 0+1+2+9 ship as PR1 (foundation + Scott's warning), Phase 3+4 ship as PR2 (init + trust), Phase 5+6+7+8 ship as PR3 (handler + loop + docs).

---

## Problem Statement

SPEC-005 as written is directionally correct but has four ambiguities that would cause divergent implementations and one missing user-facing safety affordance:

1. **P1-1 — Missing `pas.toml` behavior is undefined.** §4.2 step 1 says "reads and validates" but never specifies what happens when the file is missing, malformed, or sits in a parent directory (monorepo). Three legitimate callers (`pas run` with a quality node, `pas run` without one, `pas validate`) all need different reasonable behaviors. *This is the gap Scott flagged.*
2. **P1-2 — `cmd` execution has no trust model.** `pas.toml` is checked in; `git clone && pas run` runs whatever the file says via `std::process::Command`. We need a direnv-style first-touch prompt before this ships.
3. **P1-3 — §5.1 telemetry JSON is malformed** (raw newlines inside a JSON string literal). Downstream LLM consumers can't parse the contract as written.
4. **P1-4 — Stage ordering is implicit.** `[quality.hooks]` is an unordered TOML map; the SPEC asserts a hardcoded order with no escape hatch and no way to add a fifth stage.

Beyond P1: the SPEC's proposed handler name `pas::quality` collides with existing convention (every other handler is lowercase without a `::` namespace; we rename to `quality` in this plan), the SPEC threads vs. async (`std::process::Command` would block the tokio runtime), `serde_toml` isn't the actual crate name (it's `toml`), "two-phase" header contradicts three phases, and several other tightenings detailed in the review.

The user-facing impact of leaving these gaps unresolved: silently divergent implementations, a security footgun for users running pipelines from cloned repos, telemetry that misparses inside the very LLM loop it's meant to feed, and no signal to users when they're trying to run a quality-gated pipeline without a manifest.

---

## Proposed Solution

A nine-phase implementation, surfaced as a single Beads epic with ten child issues. The first phase is a **text-only SPEC revision** (no code) that locks the four design decisions above into the spec; everything downstream depends on it.

**Architectural shape:**

```
crates/
├── attractor-cli/
│   └── src/commands/
│       └── init.rs                ← new: `pas init` subcommand
├── attractor-pipeline/
│   └── src/
│       ├── handlers/
│       │   └── quality_handler.rs ← new: `quality` handler
│       └── validation.rs           ← extend: warn on missing pas.toml
├── attractor-quality/              ← new crate
│   └── src/
│       ├── manifest.rs             ← schema + parser
│       ├── resolution.rs           ← walk-up + missing/malformed
│       ├── trust.rs                ← ~/.config/pas/trusted.json
│       ├── detect.rs               ← toolchain detection
│       ├── enrich.rs               ← optional LLM enrichment
│       └── telemetry.rs            ← structured failure payload
```

A new `attractor-quality` crate is the right home: it isolates the manifest/trust/detection concerns from `attractor-pipeline` (which becomes a consumer via the handler), keeps `attractor-cli/src/commands/init.rs` thin, and matches the existing convention of one concern per crate (`attractor-dot` for parsing, `attractor-llm` for providers, etc.).

---

## Technical Approach

### Architecture

#### Manifest resolution (Phase 2, applied everywhere `pas.toml` is needed)

```
pas run <pipeline> [-w <workdir>]
  └── pipeline.scan_for_quality_handlers() → bool
       └── if true: attractor_quality::resolution::resolve(workdir)
            ├── Found: cache the (path, hash) on the engine
            ├── Not found: emit single startup WARN; pipeline still starts
            └── Malformed: hard error before any node executes
```

The resolver walks from `--workdir` upward, stopping at the first `.git` directory or workspace-root marker (`Cargo.toml` with `[workspace]`, `pyproject.toml`, or `package.json`). First `pas.toml` wins. No silent fallback to defaults — explicit failure with `system_guidance` that names the searched paths.

#### Trust model (Phase 4)

```
~/.config/pas/trusted.json
{
  "trusted": [
    {
      "path": "/abs/path/to/repo",
      "blake3": "<hash of pas.toml at time of trust>",
      "trusted_at": "2026-05-29T14:00:00Z",
      "source": "pas init" | "explicit prompt" | "--trust flag"
    }
  ]
}
```

Trust is keyed on `(absolute_path, blake3(pas.toml))`. Editing `pas.toml` invalidates the trust entry — next run re-prompts. `pas init` writes a trust entry as its final step. CI / non-interactive contexts use `pas run --trust` (one-shot) or `PAS_TRUST_THIS=1` env var to bypass.

**Master agent sentinel:** `PAS_AGENT=1` (settable by any orchestrator/subagent) implies `PAS_NON_INTERACTIVE=1` *and* `PAS_TRUST_THIS=1` in one shot, plus suppresses any `Editor::with_initial_text` fallback even when a TTY is detected. The three individual vars remain as fine-grained overrides for humans and CI; agents only need to set `PAS_AGENT=1`. This eliminates the integration tax of agents having to know about and individually set each prompt-suppression variable.

#### Handler dispatch (Phase 5)

The new `quality` handler lives in `crates/attractor-pipeline/src/handlers/quality_handler.rs` and registers via the existing `HandlerManager` (see `handlers/manager.rs`). It implements the same trait `codergen_handler` uses. All process execution goes through `tokio::process::Command` so the tokio runtime is not blocked during long test runs.

#### Telemetry contract (Phase 5)

Failure payload is emitted as a `serde_json::Value` (so RFC 8259 escaping is automatic), shaped to match the revised §5.1:

```rust
struct QualityTelemetry {
    status: Status,                    // pass | fail
    failed_stage: Option<String>,
    execution_metadata: ExecutionMetadata,
    context_updates: ContextUpdates,
}
struct ContextUpdates {
    last_error_log: String,            // truncated per [quality.telemetry]
    system_guidance: String,
    failure_footprint: String,         // blake3(stage + first_2KB(last_error_log))[..16]
}
```

`failure_footprint` is the explicit definition that closes P2-5: loop control compares footprints across iterations to detect "same error footprint".

#### Loop control (Phase 6)

Counter state lives in the engine's per-node context, keyed on the `quality` node ID. The counter increments each time control enters the node; resets when control enters from a different upstream node. Persisted to the checkpoint so resume preserves it (closes the "silent counter reset on resume" gap from the review).

### Implementation Phases

#### Phase 0: SPEC Revision

**Deliverable:** Updated `SPEC_PAS_INIT.md` with the following edits:
- New §4.2.0 "Manifest Resolution" specifying walk-up search, missing/malformed/parent-dir behavior.
- New §6 "Trust Model" specifying the direnv-style prompt, `~/.config/pas/trusted.json`, auto-trust on `pas init`, CI escape hatches.
- §5.1 telemetry example re-rendered as valid JSON (or YAML) with `\n`-escaped newlines; `failure_footprint` field added.
- §3 schema restructured: `stages = [...]` array + `[quality.hooks.<stage>]` sub-tables; per-stage `cwd`, `env`, `timeout_secs` fields added.
- Replace `serde_toml` → `toml` (1.x).
- Rename "two-phase" → "three-phase" (or fold TUI under Phase 2); add `--yes` / `--non-interactive` / `--no-enrich` / `--force` flag specification.
- §5.2 amendments: define `failure_footprint`, clarify counter is bound to the `quality` node ID (not the upstream node). Backoff is hardcoded to a 1-second sleep between iterations in v1 (no `backoff_factor` field).
- §4.2 amendments: `std::process::Command` → `tokio::process::Command`; remove "thread" framing.
- §2 Phase 2: define detection trigger as a decision table; specify allowlist for the LLM payload (exclude `.env*`, `*.key`, `*.pem`, `secrets/*`, `.git/`).
- Rename handler `pas::quality` → `quality` throughout.

**Success criteria:** No P1 gap left ambiguous. SPEC review re-run finds zero P1, ≤2 P2 issues.

**Estimated effort:** Small (1-2 hours).

##### Research Insights (Phase 0)

**From security-sentinel:** Add §6 trust-model wording that explicitly enumerates the threat model: (a) `pas.toml` author ≠ `pas` runner (clone scenario), (b) `pas.toml` modified by a checked-in pre-commit hook (supply-chain), (c) `pas.toml` symlinked to `/dev/random` or `/proc/*` (resource exhaustion). The SPEC should name what we are and are not defending against — this prevents future "but we never said we'd catch that" debates.

**From agent-native-reviewer:** SPEC §6 must specify `pas trust` as a first-class subcommand alongside the implicit prompt. The interactive `prompt` and the explicit `pas trust --add <path>` are two faces of the same operation; the SPEC should make that explicit so the API surface is symmetric for humans and agents.

**From best-practices-researcher:** Reference `direnv`'s `direnv allow` semantics and `mise`'s `MISE_TRUSTED_CONFIG_PATHS` env-var pattern in the SPEC's §6 informative-references block. Anchors design choices to known-good prior art.

#### Phase 1: Foundation — `attractor-quality` Crate + Manifest Schema

**Deliverable:**
- New crate `crates/attractor-quality` added to the workspace `Cargo.toml`.
- `manifest.rs` with `Manifest`, `ProjectSection`, `ToolchainSection`, `QualitySection`, `HookConfig`, `TelemetryConfig`, `LoopControl` — all `serde::Deserialize` with strict field validation.
- `toml` 1.x for the read path; `toml_edit` 0.22 for the `pas init` write path (preserves comments and key ordering on re-run).
- Unit tests for: each language template parses correctly; missing required fields produces a typed error; unknown fields produce a typed error (use `#[serde(deny_unknown_fields)]`).

**Success criteria:** `cargo test -p attractor-quality` covers happy + error paths for all three default templates from the SPEC.

**Estimated effort:** Small.

##### Research Insights (Phase 1)

**From framework-docs-researcher:** Use **`toml = "1"`** (1.1.2 latest), not 0.8 as previously specified. The 1.x release is API-stable, has `toml::Table` and `toml::de::Error` at the new paths, and uses `toml_edit` internally for round-tripping. For the `pas init` *write* path, use `toml_edit` directly so comments and key ordering survive a regeneration of an existing file. Code shape:
```rust
use toml_edit::{DocumentMut, Item, value};
let mut doc: DocumentMut = existing.parse()?;
doc["quality"]["stages"] = value(toml_edit::Array::from_iter(["format", "lint", "typecheck", "test"]));
fs::write(path, doc.to_string())?;
```

**From architecture-strategist:** Place the `Manifest` types and `serde` impls in `attractor-quality::manifest` but keep the *parsing entry point* (`Manifest::from_path`, `Manifest::from_str`) free of `std::fs` calls — pass `&str` content in. Filesystem IO belongs in `resolution.rs` so the manifest module stays pure and unit-testable without `tempfile`.

**From security-sentinel:** `#[serde(deny_unknown_fields)]` is necessary but not sufficient. Add a separate validation pass on the deserialized `Manifest` that: (a) rejects `stages` entries with duplicate names, (b) rejects empty `cmd` strings, (c) rejects `timeout_secs > 3600` (1h cap, override via env var if a user really needs it), (d) rejects `cmd` strings >4KB (defense against pathological inputs).

**From best-practices-researcher:** Adopt `cargo-make`-style stage definitions where a stage can declare `depends_on = ["format"]`. Defer to v1.1 — keep v1 sequential — but record the field name in the SPEC's "reserved future fields" so we don't paint into a corner.

#### Phase 2: Manifest Resolution (closes Scott's concern at the layer it matters)

**Deliverable:**
- `attractor_quality::resolution::resolve(start_dir) -> Result<ResolvedManifest, ResolutionError>` with three error variants: `NotFound { searched: Vec<PathBuf> }`, `Malformed { path: PathBuf, diagnostic: String }`, `Invalid { path: PathBuf, missing_fields: Vec<String> }`.
- Walk-up algorithm: stop at first `.git`, first workspace root marker, or filesystem root.
- Returned `ResolvedManifest` carries `path`, `blake3_hash`, `manifest`, `resolved_at` (for trust checks).
- Unit tests with `tempfile` fixtures: file at workdir, file at parent, no file, malformed file, file at sibling dir (should not be found).

**Success criteria:** Resolution is fully deterministic and pure (no global state). Errors carry enough context to populate `system_guidance` in telemetry.

**Estimated effort:** Small.

##### Research Insights (Phase 2)

**From security-sentinel:** The walk-up loop must canonicalize the start directory **once** at entry (`std::fs::canonicalize`) and refuse to follow symlinks during ascent (use `Path::parent` against the canonicalized path, not `fs::read_link`). This blocks the symlink-out-of-repo attack where `<workdir>/../../etc/pas.toml` resolves to an attacker file.

**From performance-oracle:** The resolver runs on every `pas run` invocation. Bound it by depth (e.g. max 16 ascents) and short-circuit at the first `.git` directory regardless of workspace markers. In practice this returns within 1ms even on cold filesystems; the bound exists to prevent pathological CI mounts (e.g. `/proc/1/root/...`) from hanging.

**From best-practices-researcher:** `direnv`'s sentinel-file pattern uses `.envrc` *plus* a parallel `.direnv/` cache directory keyed on the absolute path. We don't need the cache directory in v1 — the resolver is fast — but if Phase 6's checkpoint integration ever needs to key on "which manifest applied", we have prior art for `.pas/manifest.lock` containing the resolved absolute path + blake3 hash.

#### Phase 3: `pas init` Subcommand

Split into three Beads issues (3a/3b/3c) because they have independent surface area and 3c can ship later if needed.

##### Phase 3a — Static toolchain detection + template emission

**Deliverable:**
- `attractor_quality::detect::detect(start_dir) -> Vec<DetectedToolchain>` per the SPEC's Phase 1 signature table.
- `crates/attractor-cli/src/commands/init.rs` with `cmd_init(workdir, opts)`.
- Built-in default templates for Rust / Python / TypeScript embedded as `include_str!` from `crates/attractor-quality/templates/`.
- `--force`, `--non-interactive`, `--no-enrich`, `--dry-run` flags.
- **Init outside a git repo / workspace root.** `cmd_init` walks up from `workdir` looking for `.git` (same bounded ascent as Phase 2's resolver). Behavior:
  - `.git` found → write `pas.toml` at the discovered root.
  - No `.git` found, interactive TTY → emit WARN ("no git repo detected; writing pas.toml to current directory <cwd> — pass `--force` to confirm or `cd` into a repo first"), then prompt y/N. Default N.
  - No `.git` found, non-interactive (or `--non-interactive`) → refuse with exit code 4 (`InitError::NoWorkspaceRoot`) unless `--force` is set, in which case write to `cwd` with a single WARN line.
  - `--force` always overrides both the "no git" refusal and the "existing pas.toml" refusal.

**Success criteria:** Running `pas init` in a fixture Rust/Python/TS repo writes a parseable `pas.toml` that resolves cleanly. Running `pas init --non-interactive` in a tempdir with no `.git` exits 4 without writing; with `--force` it writes to cwd and logs the WARN.

**Estimated effort:** Medium.

##### Research Insights (Phase 3a)

**From agent-native-reviewer:** Add `pas init --dry-run` that prints the planned `pas.toml` to stdout without writing. This is the agent-native parity affordance: a UI user gets a preview in the TUI (Phase 3b), so the CLI/agent path must also have one.

**From security-sentinel:** Template emission must use `include_str!()` (compile-time embed) not `fs::read_to_string` from a path computed at runtime. Compile-time embed eliminates any "templates dir was replaced" tampering vector and guarantees the template matches the binary's expected schema.

**From architecture-strategist:** `cmd_init` should *return* an `InitPlan { manifest: Manifest, write_path: PathBuf, overwrite: OverwritePolicy }` and the actual write happens in a separate function. Makes `--dry-run` a trivial branch (return the plan, don't write) and lets tests assert the plan without IO.

**From framework-docs-researcher:** `is-terminal` (or `std::io::IsTerminal` since Rust 1.70) is the standard way to detect a TTY. Use the std variant since the MSRV story is simpler — no extra crate.

##### Phase 3b — TUI confirmation + non-interactive mode

**Deliverable:**
- TUI confirmation via `dialoguer` (lower dep footprint than `ratatui` for a single confirmation flow; if future surface grows, migrate to `ratatui` in a follow-up).
- TTY detection: if stdout is not a TTY → behave as `--non-interactive` automatically.
- Existing `pas.toml` handling: prompt to overwrite (interactive) or refuse without `--force` (non-interactive).

**Success criteria:** `pas init` in a TTY shows the discovered config and accepts edit/accept; in CI with no TTY runs straight through; existing manifest behavior matches the spec table.

**Estimated effort:** Medium.

##### Research Insights (Phase 3b)

**From framework-docs-researcher:** Pin **`dialoguer = "0.12"`** (current). 0.12 adds `with_initial_text` for `Editor`, which we want for "review the proposed manifest" flows. Also pulls in `console = "0.15"` for the terminal color stripping logic we'll reuse in telemetry truncation.

**From agent-native-reviewer:** When a TTY is detected but `PAS_NON_INTERACTIVE=1` is set, behave non-interactively. This env var should be added to the AGENTS.md guidance so subagents always set it. Belt-and-suspenders against accidental prompts in CI.

**From best-practices-researcher:** Cargo's own `cargo init` writes scaffold files and exits silently; users expect that pattern. `pas init` should match — interactive prompts only when *modifying* an existing manifest, not on first creation in an empty directory.

##### Phase 3c — Optional LLM enrichment for ambiguous/polyglot repos

**Deliverable:**
- `attractor_quality::enrich::enrich(detected, file_list) -> Option<Manifest>` triggered when ≥2 toolchains detected OR when detected language has no recognized config file.
- Filename allowlist: exclude `.env*`, `*.key`, `*.pem`, `secrets/*`, `.git/`, `node_modules/`, `target/`, `__pycache__/`.
- Reuses the existing LLM provider infrastructure in `attractor-llm`.
- `--no-enrich` flag skips this phase entirely (renamed from `--no-agent` in the deepening pass for clarity).

**Success criteria:** Polyglot fixture (Python backend + TypeScript frontend) produces a manifest with both toolchains' stages. Secret-named files do not appear in any LLM payload (assert via a captured-request test fixture).

**Estimated effort:** Medium.

##### Research Insights (Phase 3c)

**From security-sentinel:** The allowlist is the wrong shape — it's a denylist (excluding `.env*`, `*.key`, etc.). Invert it to a positive allowlist: only filenames matching `(Cargo\.toml|package\.json|pyproject\.toml|tsconfig.*\.json|requirements.*\.txt|setup\.(py|cfg)|pnpm-lock\.yaml|yarn\.lock|poetry\.lock|.gitignore|README\.md|Makefile)` are sent. Any new ecosystem file (`pixi.toml`, `bun.lockb`, etc.) requires explicit allowlist update. Denylists rot; allowlists fail closed.

**From security-sentinel + agent-native-reviewer (resolved — flag is `--no-enrich`):** The original draft used `--no-agent`, which read ambiguously (does it disable agent integration entirely?). The deliverable above uses `--no-enrich` because it names what is actually skipped. No further action.

**From code-simplicity-reviewer:** This phase is the largest single complexity contributor in the plan. If timeline pressure hits, this is the cut. Document the cut-path explicitly: "If Phase 3c is deferred, `pas init` falls back to emitting a multi-language manifest stub with all detected stage blocks and a `# TODO: review and consolidate` comment header. User edits manually." That fallback should ship in 3a regardless, so 3c is purely an enhancement.

**From best-practices-researcher:** Cap LLM payload at ~8 KB total (filename list + structural summary). LLM enrichment is a hint, not authoritative — if the model errors or times out, fall back to the 3a multi-stage stub. Document this fallback path so the failure mode is "user gets the unenriched manifest", never "init fails".

#### Phase 4: Trust Model

**Deliverable:**
- `attractor_quality::trust` module: `is_trusted(path, blake3) -> bool`, `add_trust(path, blake3, source)`, `prompt_and_add(path, blake3)`, `remove_trust(path)`.
- Trust file at `$XDG_CONFIG_HOME/pas/trusted.json` (fallback `~/.config/pas/trusted.json`).
- `pas init` calls `add_trust(..., source = "pas init")` as the final step (closes the loop: a user-authored manifest is trusted automatically).
- `pas run --trust` flag for one-shot trust without persisting.
- `PAS_TRUST_THIS=1` env var for CI.
- Trust prompt is suppressed in non-TTY contexts unless `--trust` or `PAS_TRUST_THIS=1` is set, in which case the run aborts with a clear "untrusted manifest, pass --trust or run pas init" error.
- **Distinct exit codes** at trust failure (applies to `pas run` and to all `pas trust` subcommands):
  - `exit_code = 2` — `TrustError::Untrusted` (trust file readable, entry not present or hash mismatch). Recoverable: user can run `pas trust --add` or pass `--trust`. `pas trust --remove <path>` for a path that isn't in the store also exits 2.
  - `exit_code = 3` — `TrustError::CorruptedStore` (trust file present but malformed JSON / unreadable / permission-denied). Non-recoverable without manual intervention; error message names the file path and parse error. Any `pas trust` subcommand (`--add`, `--remove`, `--list`) that observes a corrupted store also exits 3.
- **`pas trust --remove` during a live pipeline:** the in-process `OnceCell` cache (see Research Insights below) is snapshot at engine startup. A `pas trust --remove <path>` issued in a separate terminal mid-run does **not** invalidate the in-flight pipeline's trust state — the next `pas run` re-reads the file and observes the removal. This is documented in `docs/cli-reference.md` and in the help text of `pas trust --remove`. Rationale: cross-process invalidation requires file watchers and adds complexity for a vanishingly rare ops scenario.

**Success criteria:** First `pas run` against an unseen `pas.toml` prompts in a TTY and aborts in non-TTY. Re-running with no manifest changes does not re-prompt. Editing `pas.toml` invalidates trust. Corrupted trust file produces exit 3 (not exit 2). `pas trust --remove` during a live run is a no-op for the running pipeline (asserted via integration test).

**Estimated effort:** Medium.

##### Research Insights (Phase 4)

**From framework-docs-researcher:** Pin **`directories = "6"`** (current major). Alternatively migrate to **`etcetera`** which has better Windows defaults (uses `%LOCALAPPDATA%\pas\trusted.json` instead of `%APPDATA%\pas\config\trusted.json`) and avoids the `dirs-sys` transitive dep. Decision: stay with `directories = "6"` for now (smaller diff); migrate in v1.1 if cross-platform issues surface.

**From performance-oracle:** Cache the parsed trust set behind a `tokio::sync::OnceCell<TrustSet>` keyed on the engine lifetime. A 10-stage loop with a `quality` node in the middle would otherwise re-parse `trusted.json` 10× per iteration. Invalidate the cache on `pas trust --add/--remove` (in-process); cross-process invalidation is not a concern because trust changes are explicit user operations.

**From performance-oracle:** Write `trusted.json` via `tempfile::NamedTempFile` in the same directory, then `persist()` for an atomic rename. Two concurrent `pas init` invocations (rare but possible — e.g. dev machine + remote session) won't produce a corrupted half-written file.

**From security-sentinel:** Trust file permissions: `chmod 0600` on Unix (sensitive — contains absolute paths of every PAS project). On Windows, set explicit ACL deny for non-owner. The trust file is not a secret per se but it does leak the user's project layout if shared.

**From agent-native-reviewer:** `pas trust` becomes a first-class subcommand: `pas trust --add <path>`, `pas trust --remove <path>`, `pas trust --list`. The interactive prompt remains for humans, but agents/CI use the explicit command. Adds ~30 lines to `attractor-cli` and removes ambiguity for autonomous workflows.

#### Phase 5: `quality` Handler

**Deliverable:**
- New `crates/attractor-pipeline/src/handlers/quality_handler.rs` registered in `handlers/manager.rs` under handler name `quality`.
- Sequential stage execution per `[quality.stages]` order; `allow_failure = true` logs WARN and proceeds; `allow_failure = false` aborts the loop.
- All process execution via `tokio::process::Command` with `kill_on_drop(true)` to prevent orphans on pipeline abort.
- Per-stage timeout enforced via `tokio::time::timeout`.
- Output truncation: respect `truncate_logs_after_bytes` from `[quality.telemetry]` using a **head-and-tail** strategy — keep first 25% of bytes + last 75%, joined by `... [<N> bytes truncated] ...`. Implemented as a bounded ring buffer fed by `tokio::io::AsyncBufReadExt::lines()`. Hardcoded v1; no `truncate_strategy` user-facing field (deferred per simplicity review).
- Telemetry payload built per the revised §5.1 contract; `failure_footprint = blake3(failed_stage || "|" || first_2KB(last_error_log))[..16]` (BLAKE3 chosen over SHA-256 for ~10× speed at the same collision resistance for our truncation window).
- Trust check happens at handler invocation, not at engine startup, so untrusted manifests fail loudly at the point of harm.
- **AGENTS.md updated in this phase** (moved up from Phase 8 per agent-native-reviewer): adds the `quality` handler contract, the `PAS_AGENT`/`PAS_NON_INTERACTIVE`/`PAS_TRUST_THIS` env-var table, and the structured-tracing event codes (`QUALITY_NO_MANIFEST`, etc.) so any subagent built during Phases 6/7/8 has the handler contract available.

**Success criteria:** Unit tests for stage runner (happy / allow_failure / hard fail / timeout). Integration test in `crates/attractor-pipeline/tests/integration.rs` that runs a 2-node pipeline (`generate → quality`) against a fixture Rust crate and asserts pass + fail outcomes.

**Estimated effort:** Large.

##### Research Insights (Phase 5)

**From security-sentinel:** Process spawn must call `env_clear()` followed by `envs()` with an explicit allowlist: `PATH`, `HOME`, `LANG`, `LC_ALL`, `TZ`, `USER`, `CARGO_HOME`, `RUSTUP_HOME`, `PNPM_HOME`, `NPM_CONFIG_*`, `PYTHONPATH`, `VIRTUAL_ENV`, plus any per-stage `env = { ... }` from the manifest. Prevents inheriting `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_*`, `BASH_ENV`, or other env-based injection vectors from a malicious shell.

**From security-sentinel:** Two `cmd` shapes supported by the manifest: `cmd = "cargo test --release"` (shell-style, splits via `shlex::split` — fail closed on parse error) and `cmd_argv = ["cargo", "test", "--release"]` (explicit argv, no shell). Default to argv when both are present; document the precedence. Avoid `sh -c` entirely — it's a footgun for quoting and a vector for command injection if any field ever sees env interpolation.

**From security-sentinel:** Run the parsed argv through a denylist scanner *once at trust-grant time* — `rm -rf`, `sudo`, `doas`, `mkfs`, `dd if=`, `:(){ :|:& };:`, `curl ... | sh`, `wget ... | sh`, `bash <(curl ...)`. Match by canonicalized argv[0] basename + argv pattern. If any match, the trust prompt requires a typed confirmation (not just Enter) and the matched line is echoed back. Per-invocation re-scanning was considered but cut for v1: the hash check already detects post-trust modification.

**From performance-oracle:** Stream stdout/stderr line-by-line into a ring buffer instead of collecting `Vec<u8>`. Use `tokio::io::AsyncBufReadExt::lines()` on each handle, feed into a bounded `VecDeque<String>` with a byte-count check on each push. Memory bounded to `truncate_logs_after_bytes` *exactly*, regardless of test verbosity. Without this, a chatty `cargo test --verbose` can balloon to hundreds of MB before truncation kicks in.

**From performance-oracle:** Head-and-tail truncation, not head-only. Errors usually appear at the end of test output, but the *trigger* (the failing test name, the panic line) is often surrounded by setup context at the start of the panic. Keep first 25% of bytes + last 75%, joined by `... [<N> bytes truncated] ...`. Hardcoded v1; no `truncate_strategy` knob (deferred per simplicity review).

**From performance-oracle + framework-docs-researcher:** Spawn each stage in its own process group via `tokio::process::Command::process_group(0)` (stable in tokio 1.40+). On timeout, ctrl-C, or pipeline abort, call `nix::sys::signal::killpg(pgid, SIGTERM)` to terminate the entire group. `kill_on_drop(true)` alone leaks `cargo test`'s spawned test binaries; `process_group` + `killpg` is the fix. Add `nix = "0.29"` to deps with the `signal` feature flag.

**From performance-oracle:** Strip ANSI escape codes from captured output before computing `failure_footprint` so footprints aren't sensitive to terminal width / color settings. Use `console::strip_ansi_codes` (transitively present via `dialoguer`).

**From architecture-strategist:** `telemetry.rs` should live in `attractor-quality`, not the handler. The handler builds a `QualityRunOutcome` and hands it to `attractor_quality::telemetry::format_for_llm(&outcome)`. Keeps the handler thin (orchestration only) and lets telemetry be unit-tested without spawning processes.

**From agent-native-reviewer:** `system_guidance` should be stage-aware. Default templates:
- `format`: "Run `pas init` to regenerate format settings, or fix whitespace/style with `<format_cmd> --write`."
- `lint`: "Address the lint warnings shown above. Do not silence with `#[allow(...)]` unless the warning is genuinely incorrect."
- `typecheck`: "Read the type error and fix the source. Do not change the types just to make the error go away — fix the underlying mismatch."
- `test`: "Read the failing test names and assertions. Fix the production code, not the tests. If the test is genuinely wrong, mark it as `#[ignore]` with a comment explaining why."

**From code-simplicity-reviewer:** The stage runner is the right place to invest in clarity. Extract `run_stage(stage: &HookConfig, env: &Env) -> StageOutcome` as a pure-ish function with a `tokio::process::Command` factory injected. Makes the loop body ~20 LOC and tests ~5 LOC each.

**From best-practices-researcher:** LangGraph's "observation fingerprint" pattern matches what `failure_footprint` does. Simplification: use BLAKE3 (16-byte truncated digest of `stage || "|" || first_2KB(stderr)`). `blake3` is one new workspace dep — but it replaces what would otherwise have been two (a hash crate for trust + a hash crate for footprint). ~10× faster than SHA-256 and the truncation doesn't change collision resistance for our use case.

#### Phase 6: Loop Control + Checkpoint Integration

**Deliverable:**
- Per-(quality-node-ID, upstream-edge) counter map in the engine's `NodeContext`.
- Counter increments on each entry from the same upstream node; resets when control enters from a different upstream node.
- Counter and last-`failure_footprint` are written to the checkpoint (`crates/attractor-pipeline/src/checkpoint.rs`) so resume preserves both.
- Hardcoded 1-second `tokio::time::sleep` between iterations (applied before the second and subsequent iterations). No `backoff_factor` field in v1 — the configurable knob was cut per simplicity review; the checkpoint `schema_version` covers forward-compat if we add it later.
- On iteration ≥2, the engine prepends the operational warning from SPEC §5.2 step 2 to the downstream node's prompt context.
- On iteration > `max_fix_iterations`, the engine aborts the pipeline cleanly with `exit_code = 1` and a final log entry naming the unhealing stage.

**Success criteria:** Looped pipeline fixture demonstrates: (a) successful self-correction within budget, (b) clean abort at budget, (c) resume from checkpoint preserves the counter (verified via a forced kill + re-run test).

**Estimated effort:** Large.

##### Research Insights (Phase 6)

**From architecture-strategist:** Add `schema_version: u32` to the checkpoint root struct. Default via `#[serde(default = "default_schema_version")]`. When the loop-counter shape evolves (e.g. to add per-edge tracking), bump and write a migration in `checkpoint::migrate()`. Without this, every schema evolution forces users to `--fresh`, which loses in-flight pipeline state.

**From performance-oracle:** Checkpoint writes should batch — currently every node completion writes the file. With a `quality` node firing every loop iteration, that's N writes for N iterations. Buffer the in-memory checkpoint and flush at: (a) loop-control state changes, (b) successful pipeline transitions, (c) every 5 seconds (whichever first). On Ctrl-C, the engine's existing shutdown hook flushes once more.

**From best-practices-researcher:** LangGraph and Temporal both key retry counters on `(node_id, attempt_uuid)` not `(node_id, upstream_edge_id)`. Our edge-based key is correct for *this* model (resetting on a different upstream) but document the reasoning in code comments — future-us will be tempted to "simplify" it.

**From agent-native-reviewer:** The retry-warning prompt prepend on iteration 2+ should be a *separate context block* with a sentinel, not inline prose. Format:
```
<retry-warning iteration="2" prev-footprint="abc123">
This is your second attempt to fix the failed quality stage 'typecheck'. The previous attempt produced an identical error footprint. Try a structurally different approach.
</retry-warning>
```
Sentinels let downstream agents parse the warning without false positives from prose mentioning "previous attempt".

**From code-simplicity-reviewer (applied — `backoff_factor` cut from v1):** The original SPEC defined `backoff_factor` as a sleep multiplier with a no-op default of `1.0`. A no-op default is speculative complexity — we hardcoded a 1-second sleep instead and removed the field from the manifest schema. If a user later requests configurable backoff, the checkpoint's `schema_version` lets us add the field without forcing `--fresh`.

#### Phase 7: Validation Rule + Startup Warning (Scott's concern, end-user facing)

**Deliverable:**
- New **preflight check** in `crates/attractor-pipeline/src/preflight.rs` (new module, distinct from `validation.rs` per architecture-strategist insight — keeps `pas validate` pure-by-default; preflight is the IO-bearing layer that runs after parsing and before execution). When a pipeline uses `handler="quality"`, attempt `attractor_quality::resolution::resolve(workdir)`. If `NotFound`, emit a single structured `WARN`-level tracing event at pipeline-load time with `code = "QUALITY_NO_MANIFEST"`, `workdir`, and `suggestion = "pas init"`:
  > `WARN: pipeline 'X.dot' uses the 'quality' handler but no pas.toml was found at <workdir>. Run 'pas init' to generate one. The quality node will fail when reached.`
- The warning fires from `pas run` always; from `pas validate` only with the `--preflight` flag (default `pas validate` stays pure/syntactic).
- The warning does **not** fire when no node uses the `quality` handler (silence is golden).
- The warning is a `WARN`, not an `ERROR` — pipelines without quality nodes still run (closes the original ambiguity in the SPEC's §4.2).
- **Resolution result is cached on the `RunContext`**, not re-resolved at the handler. Preflight (this phase) calls `attractor_quality::resolution::resolve(workdir)` exactly once at pipeline-load; the result (`Result<ResolvedManifest, ResolutionError>`) is stored on the existing `RunContext` as `quality_manifest: Option<Result<ResolvedManifest, ResolutionError>>` and read by the Phase 5 handler. The handler **must not** call `resolve()` itself — it pattern-matches on the cached result. Rationale: one filesystem walk per `pas run`, not one per `quality` node per loop iteration; also guarantees preflight and handler see identical state (no TOCTOU between the warning and execution).

**Success criteria:** Three integration tests in `attractor-pipeline/tests/`: (a) pipeline with quality + manifest → no warn, (b) pipeline with quality without manifest → exactly one warn line, (c) pipeline without quality without manifest → no warn. Plus: assert via tracing capture that `attractor_quality::resolution::resolve` is invoked exactly once for a 10-iteration loop containing 3 `quality` nodes.

**Estimated effort:** Small.

##### Research Insights (Phase 7)

**From architecture-strategist:** Introduce a new `pipeline::preflight` module distinct from `pipeline::validation`. Reasoning: `validation` was intended as pure/syntactic (graph well-formedness, edge types, handler-name resolution). Mixing IO checks into validation breaks `pas validate`'s implied contract (pure, deterministic, no env access). Preflight runs after validation but before execution, and its checks are explicit IO. New signature: `pipeline::preflight::run(graph: &Graph, workdir: &Path) -> Vec<PreflightFinding>`. The missing-`pas.toml` warning lives here. `pas validate` gains a `--preflight` flag to opt in; default stays pure.

**From agent-native-reviewer:** The WARN must be machine-parseable for agents. Wrap in a structured tracing event:
```rust
tracing::warn!(
    target: "pas::preflight",
    code = "QUALITY_NO_MANIFEST",
    workdir = %workdir.display(),
    suggestion = "pas init",
    "pipeline 'X.dot' uses the 'quality' handler but no pas.toml was found at {workdir}. Run 'pas init' to generate one. The quality node will fail when reached."
);
```
The `code = "QUALITY_NO_MANIFEST"` field is the contract: agents grep on the code, humans read the message.

**From performance-oracle:** Cache the resolution result so the preflight + handler-invocation path don't both hit the filesystem. Pass the `Option<ResolvedManifest>` down through the engine's `RunContext`.

**From code-simplicity-reviewer:** The "WARN does not fire when no quality node is present" check is the simplest possible filter: a pre-pass over `graph.nodes()` filtering by `handler == "quality"`, returning early if none. ~3 LOC. Don't over-engineer with traits or registries here.

**From best-practices-researcher:** This is the user-facing affordance that closes Scott's specific concern. Add an example to the README and `docs/guide.md` showing exactly what the warning looks like and what the fix is. Documentation here matters more than code volume.

#### Phase 8: Docs + Integration Tests

**Deliverable:**
- New section in `docs/cli-reference.md` for `pas init` (synopsis, flags, examples) and updated entry for `pas run` documenting the missing-`pas.toml` warning.
- **Exit code reference table** in `docs/cli-reference.md`: `0` success, `1` loop exhausted / pipeline aborted, `2` untrusted manifest (recoverable via `pas trust --add` or `--trust`), `3` trust file corrupted (manual intervention required), `4` `pas init` outside workspace without `--force`.
- (AGENTS.md content was already landed in Phase 5 — see that phase's deliverable for the `quality` handler contract and env-var table.)
- End-to-end integration test exercising the full chain: `pas init` in tempdir fixture → `pas run` of a pipeline with a `quality` node → assert all stages execute → mutate fixture to break a stage → assert loop fires → assert clean abort at `max_fix_iterations`.

**Success criteria:** `cargo test --workspace` passes. `pas init && pas run` round-trips against fixture repos for all three default languages.

**Estimated effort:** Medium.

##### Research Insights (Phase 8)

**From agent-native-reviewer:** AGENTS.md should be updated *in Phase 5* (when the handler lands), not Phase 8. Reasoning: any subagent spawned to build Phase 6/7/8 will benefit from the handler-contract documentation being present. Move the AGENTS.md section out of Phase 8 and into Phase 5's deliverables; Phase 8 keeps the broader docs + end-to-end test sweep.

**From best-practices-researcher:** The end-to-end test should run against actual `git`-initialized fixture crates (not just `tempfile` directories). The walk-up resolver stops at `.git`, so testing the stop semantics requires real `.git` markers. Shell out to `git init` (`std::process::Command::new("git").arg("init").current_dir(tmp).status()`) in `tests/fixtures/` setup — the `git2` crate was considered but rejected for the dev-dep weight when one process call suffices.

**From architecture-strategist:** Add a CI matrix dimension for Linux/macOS for the trust-file tests (XDG path resolution differs). Windows is best-effort in v1 — document non-coverage.

**From code-simplicity-reviewer:** The end-to-end test for "loop fires until exhaustion" is the most expensive integration test in the suite (spawns subprocess loops). Mark with `#[ignore]` by default; expose via `cargo test --ignored` or a `e2e_loop_test` feature flag. Keeps `cargo test` under a minute for the inner dev loop.

---

## Alternative Approaches Considered

| Alternative | Why rejected |
|---|---|
| **Keep SPEC's inline `[quality.hooks]` form** | Schema lock-in. Adding `cwd`/`env`/`timeout_secs` later inflates every line; adding a 5th stage requires code changes. |
| **Binary allowlist for `cmd`** instead of trust prompt | Less ergonomic. Forces every project's tooling onto a hardcoded list; `pnpm`, `bun`, `uv`, `pdm`, `cargo-nextest` would all need allowlist updates. |
| **Defer trust model entirely to v1.1** | Real footgun: `git clone && pas run` against an attacker-authored `pas.toml` runs arbitrary commands. The cost of a release-note warning is roughly zero; the cost of a CVE is not. |
| **Handler name `pas::quality`** | Introduces `::` as a new dispatch syntax that nothing else uses. The lowercase `quality` matches the existing convention without ambiguity. |
| **Use `ratatui` for TUI confirmation** | Heavyweight for a single confirm/edit flow. `dialoguer` covers it in <50 lines. Reserve `ratatui` for a future `pas dashboard` if we ever need a full TUI. |
| **Hard-fail at `pas run` startup when manifest is missing** | Breaks pipelines that don't use the `quality` handler. The warn-then-fail-at-handler approach gives the user a signal without blocking unrelated work. |
| **`#[serde(default)]` for missing manifest fields** | Hides typos. Strict `deny_unknown_fields` + explicit required-field validation catches more user errors at parse time. |
| **One Beads issue per phase** (9 issues) | Some phases are too small to be standalone. Consolidating Phases 1+2 into one issue and Phases 7 into one issue lands at 10 child issues — granular enough to ship incrementally but not so fragmented that tracking overhead dominates. |

---

## System-Wide Impact

### Interaction Graph

```
pas run <pipeline> -w <workdir>
  └── load_pipeline()                                [attractor-cli]
       └── PipelineGraph::from_dot()                 [attractor-pipeline]
       └── validation::run_all(graph)                [attractor-pipeline]  (pure/syntactic)
       └── preflight::run(graph, workdir)            [attractor-pipeline]  ← NEW (Phase 7)
            └── check_quality_manifest()             ← NEW (Phase 7)
                 └── attractor_quality::resolution::resolve()  ← ONLY filesystem walk per `pas run`
                      ├── (Ok)         → store on RunContext.quality_manifest
                      ├── (NotFound)   → emit WARN once + store Err on RunContext
                      └── (Malformed)  → store Err on RunContext (handler surfaces it later)
  └── Engine::execute(graph)                         [attractor-pipeline]
       └── HandlerManager::dispatch("quality")       [attractor-pipeline]
            └── QualityHandler::run(node, context)   ← NEW (Phase 5)
                 ├── match context.quality_manifest   ← READS CACHE (no re-resolve)
                 │    ├── Some(Ok(m))      → proceed with m
                 │    └── Some(Err(e))     → return Fail with system_guidance derived from e
                 ├── attractor_quality::trust::is_trusted()
                 │    └── (untrusted) → prompt OR fail in non-TTY
                 ├── for stage in manifest.quality.stages:
                 │    └── tokio::process::Command::spawn() + timeout
                 │         └── (fail + allow_failure=false) → break
                 ├── attractor_quality::telemetry::build()
                 │    └── compute failure_footprint
                 ├── checkpoint::persist(node_id, counter, footprint)
                 └── return outcome
       └── edge_selection::choose() → fail edge to upstream node
            └── LoopControl::increment(node_id, upstream_id) ← NEW (Phase 6)
                 ├── (counter > max) → abort pipeline
                 ├── (counter == 2) → prepend retry warning
                 └── (footprint matches prev) → escalate prompt
```

Two-level chain reactions worth noting:
1. **Validation → resolution → IO.** The new validation rule does real filesystem IO at pipeline load. This is a deliberate tradeoff (early warning) but means `pas validate` is no longer pure. Document this in the rule's doc comment.
2. **Handler → checkpoint → loop counter.** A pipeline that hits the `quality` handler, fails, gets killed (Ctrl-C), and is re-run will resume with the counter intact. If we mishandle this, runaway loops can hide behind resume. Phase 6 must include the checkpoint round-trip test.

### Error & Failure Propagation

| Origin | Error type | Handler | Surfaced as |
|---|---|---|---|
| `pas.toml` missing | `ResolutionError::NotFound` | validation rule (warn) + quality handler (fail) | Startup `WARN` + node `Fail` with `system_guidance` |
| `pas.toml` malformed | `ResolutionError::Malformed` | quality handler (fail) | Node `Fail` with diagnostic in `last_error_log` |
| Stage `cmd` non-zero | `ExecError::NonZeroExit` | quality handler | Node `Fail` with formatted telemetry; loop control engages |
| Stage timeout | `ExecError::Timeout` | quality handler | Node `Fail` with timeout-specific `system_guidance` |
| Trust check fails (non-TTY): entry missing or hash mismatch | `TrustError::Untrusted` | quality handler | Hard error: pipeline aborts with `exit_code = 2` (recoverable — user can `pas trust --add` or pass `--trust`) |
| Trust file corrupted / unreadable | `TrustError::CorruptedStore` | quality handler | Hard error: pipeline aborts with `exit_code = 3`, message names the file path + parse error (non-recoverable without manual intervention) |
| Loop counter exceeded | `LoopError::Exhausted` | engine | Pipeline aborts with `exit_code = 1` and final summary line |
| `pas init` outside a git repo, non-interactive, no `--force` | `InitError::NoWorkspaceRoot` | init command | Refuses to write; exits 4 with a "no workspace root found" message |
| Tokio task panic | `JoinError` | engine | Pipeline aborts; existing engine handling applies |

No retry strategies conflict because the loop control is the only retry mechanism in this pipeline — the underlying `tokio::process::Command` is not retried by us.

### State Lifecycle Risks

Three persistent state items, each with a clear invariant:

| State | Location | Invariant | Risk if violated |
|---|---|---|---|
| Trust entries | `~/.config/pas/trusted.json` | One entry per `(abs_path, blake3)` pair | Stale entries → false trust after edits. Mitigation: hash-based key invalidates on edit. |
| Loop counter | Checkpoint file (per-pipeline) | Counter monotonic within a checkpoint generation; reset on `--fresh` | Counter not persisted → runaway loop after Ctrl-C. Mitigation: explicit test in Phase 6. |
| Pipeline checkpoint | `.pas/logs/<pipeline>-<hash>/checkpoint.json` | Existing checkpoint schema | Adding new fields could break resume of old checkpoints. Mitigation: `#[serde(default)]` on new checkpoint fields. |

### API Surface Parity

The `quality` handler is the only consumer of `pas.toml` at runtime. `pas init` writes the file. `pas validate` reads it via the new validation rule. No other CLI subcommand interacts with the manifest, which keeps the surface tight. This is worth preserving — resist requests to add `--quality-stage <name>` flags to `pas run`; if a stage subset is needed, the right answer is conditional `[quality.hooks]` overrides via environment-variable interpolation in v1.1.

### Integration Test Scenarios

Five cross-layer scenarios that unit tests with mocks won't catch:

1. **Init → resolve → trust → run, all green.** `pas init` in a fixture Rust crate, then `pas run` of a 2-node pipeline. Asserts: manifest written, trust registered, all stages execute, exit 0.
2. **Init → mutate → run, trust invalidated.** `pas init`, edit `pas.toml`, then `pas run` in non-TTY. Asserts: trust invalidated by hash mismatch, run aborts with clear "edited, please re-trust" error.
3. **No init → run pipeline with quality node, missing manifest.** Asserts: exactly one startup WARN, pipeline starts, quality node fails with `system_guidance` pointing at `pas init`.
4. **Loop fires until exhaustion.** Pipeline with `quality` → `codergen` → `quality`. Fixture is intentionally broken so every iteration fails with the same footprint. Asserts: counter increments, retry warning appended on iteration 2, clean abort at `max_fix_iterations` with non-zero exit.
5. **Checkpoint resume preserves counter.** Run the loop scenario, kill after iteration 2, re-run without `--fresh`. Asserts: counter resumes at 2 (not 0), aborts after one more iteration instead of three.

---

## Acceptance Criteria

### Functional Requirements

- [ ] SPEC-005 revised per Phase 0; review re-run finds zero P1 issues.
- [ ] `pas init` writes a parseable `pas.toml` for Rust, Python, and TypeScript fixture repos.
- [ ] `pas init --non-interactive` works without a TTY.
- [ ] `pas init` re-run refuses to overwrite without `--force`.
- [ ] `pas init --no-enrich` skips LLM enrichment even for polyglot repos.
- [ ] `pas init --dry-run` prints the planned `pas.toml` to stdout without writing.
- [ ] `pas run --dry-run` runs preflight + trust check + first-stage command resolution but stops before spawning.
- [ ] `PAS_NON_INTERACTIVE=1` suppresses prompts even when a TTY is detected.
- [ ] `PAS_AGENT=1` master sentinel implies `PAS_NON_INTERACTIVE=1` + `PAS_TRUST_THIS=1` (single env var for orchestrators).
- [ ] `pas init` auto-trusts the manifest it writes.
- [ ] `pas run` against a pipeline with a `quality` node and no `pas.toml` emits exactly one startup WARN and fails at the quality node with `system_guidance`.
- [ ] `pas run` against a pipeline with no `quality` node and no `pas.toml` emits no warnings (silence preserved).
- [ ] `pas run` against an untrusted manifest prompts in a TTY and aborts in non-TTY.
- [ ] `pas run` against a trusted manifest runs all stages in `[quality.stages]` order.
- [ ] `allow_failure = true` stages log WARN and proceed; `allow_failure = false` stages abort the handler.
- [ ] Loop control increments the counter on each entry from the same upstream, prepends a retry warning on iteration 2+, and aborts cleanly at `max_fix_iterations`.
- [ ] Loop counter persists across checkpoint resume.
- [ ] Telemetry JSON is RFC 8259 valid (`failure_footprint` present).

### Non-Functional Requirements

- [ ] All process execution uses `tokio::process::Command` (no `std::process::Command` in handler hot path).
- [ ] Stage timeout enforced via `tokio::time::timeout` with `kill_on_drop(true)`.
- [ ] Trust file uses XDG-compliant path resolution.
- [ ] LLM enrichment payload excludes `.env*`, `*.key`, `*.pem`, `secrets/*`.
- [ ] Output truncation uses head-and-tail strategy (first 25% of bytes + last 75%, joined by `... [<N> bytes truncated] ...`), bounded by `truncate_logs_after_bytes`.
- [ ] `cargo fmt --check && cargo clippy --workspace -- -D warnings` passes.

### Quality Gates

- [ ] Unit test coverage on `attractor-quality` crate ≥ 80%.
- [ ] All five integration test scenarios above pass.
- [ ] `docs/cli-reference.md` updated with `pas init` section and `pas run` warning note.
- [ ] `AGENTS.md` updated with `quality` handler reference.
- [ ] Two reviewers: one for SPEC revision (Phase 0), one for full implementation review at Phase 6 milestone.

---

## Success Metrics

| Metric | Target | Measurement |
|---|---|---|
| Init success rate | ≥ 95% on fixture repos (Rust/Python/TS/polyglot) | Integration test pass rate |
| Time-to-first-`pas-run` after clone | ≤ 60s in interactive mode (`pas init && pas run pipelines/x.dot`) | Manual benchmark |
| False-positive WARNs | 0 (warn must not fire when no `quality` node is in the pipeline) | Test scenario #3 |
| Trust prompt invasiveness | 1 prompt per `(repo, manifest-hash)` lifetime | Manual UX check |
| Loop containment | 100% (no runaway loop should escape `max_fix_iterations` across checkpoint resume) | Test scenario #5 |

---

## Dependencies & Prerequisites

**Crate dependencies (new):** *(post-review cuts applied — sha2 and git2 dropped, one hash crate for both uses)*
- `toml = "1"` (1.1.2 current; `toml::Table` and `toml::de::Error` at new paths) — manifest read path
- `toml_edit = "0.22"` — manifest *write* path; preserves comments and key ordering on re-run of `pas init`
- `dialoguer = "0.12"` — TUI confirmation; `Editor::with_initial_text` for manifest preview/edit flow
- `console = "0.15"` (transitive via dialoguer; pinned explicitly because we use `strip_ansi_codes` in telemetry)
- `blake3 = "1"` — used for **both** trust hash (`(absolute_path, blake3(pas.toml))` keying) and `failure_footprint`. One hash for the whole feature.
- `directories = "6"` — XDG path resolution
- `nix = { version = "0.29", features = ["signal"] }` — Unix process-group signaling via `killpg` for orphan-free stage cancellation
- `shlex = "1"` — `cmd` string splitting in the shell-style manifest form
- `tempfile = "3"` (dev-dep + runtime) — fixtures + atomic trust-file writes

**Test fixtures use shell-out, no extra crate:** initializing `.git` markers for the walk-up resolver tests uses `std::process::Command::new("git").arg("init").current_dir(...)` in the test setup. No `git2` dep needed.

**Std-library APIs to use** (no new crate needed):
- `std::io::IsTerminal` (Rust 1.70+) — TTY detection, supersedes the `is-terminal` crate

**Existing dependencies to leverage:**
- `serde` / `serde_json` — already in workspace
- `tokio` — already in workspace (`#[tokio::main]` in `attractor-cli`)
- `tracing` / `tracing-subscriber` — already in workspace for logs
- `clap` — already in `attractor-cli`
- `attractor-llm` provider abstraction — for Phase 3c enrichment

**Internal prerequisites:**
- Phase 0 SPEC revision before any code phase begins.
- Phase 1 + 2 (foundation + resolution) before Phases 3, 5, 7.
- Phase 4 (trust) before Phase 5 finalization (handler must check trust).
- Phase 5 (handler) before Phase 6 (loop control runs inside the handler scope).

---

## Risk Analysis & Mitigation

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Trust prompt blocks CI runs | High | Medium | `PAS_TRUST_THIS=1` env var + `--trust` flag documented prominently in `cli-reference.md` |
| Checkpoint schema break for existing pipelines | Medium | High | New checkpoint fields all `#[serde(default)]`; resume of pre-feature checkpoints assumed counter=0 |
| LLM enrichment costs spiral on large repos | Medium | Medium | Payload size cap (256 entries max, ≤8 KB); `--no-enrich` flag; respect existing `--max-budget-usd` |
| Cross-platform TUI behavior (Windows/WSL) | Low | Low | `dialoguer` handles fallback; explicit non-TTY detection; integration tests run on macOS + Linux |
| `tokio::process::Command` orphans on Ctrl-C | Medium | High | `kill_on_drop(true)` on every spawned command; existing `Drop` semantics in engine |
| Footprint collisions hide distinct failures | Low | Medium | Footprint is `blake3(stage \|\| first_2KB(error_log))[..16]`; collision probability ≈ 2^-64 per pair |
| `pas init` overwrites a hand-edited `pas.toml` | Medium | High | Refuse without `--force` (non-interactive); prompt explicitly in interactive mode |
| Manifest changes silently invalidate trust | Low | Low | Document hash-based invalidation; trust prompt explains why on re-trigger |
| Polyglot detection wrong (e.g., `package.json` in Python deps dir) | Medium | Low | Detection only searches workdir root, not recursively; documented |
| Scope creep into "what if the user wants conditional stages" | Medium | Medium | Deferred to v1.1; documented in "Future Considerations" |

---

## Resource Requirements

- **Engineering:** ~2-3 weeks of focused work for one engineer; ~1-2 weeks with subagent parallelism on Phase 3a/3b/3c, Phase 5, Phase 7 (independent test fixtures).
- **Review:** One reviewer for Phase 0 (SPEC); one reviewer at Phase 6 (mid-implementation); one final review pre-merge.
- **Infrastructure:** No new infra; existing `cargo` workspace + `tokio` runtime.
- **External services:** Phase 3c uses the existing LLM provider abstraction — no new vendor.

---

## Future Considerations

Defer to v1.1+:
- **Per-stage conditional execution** via `[quality.hooks.<stage>.when]` expressions.
- **Environment variable interpolation** in `cmd` strings (`${HOME}`, `${PAS_WORKDIR}`).
- **Cache layer for `pas init` detection** to avoid re-scanning on hot reload.
- **`pas dashboard` TUI** showing trust state, loop counters, and recent run history — would justify migrating from `dialoguer` to `ratatui`.
- **Cross-pipeline shared trust** (workspace-level trust instead of per-repo) — needs broader UX design.
- **`pas init --upgrade`** to migrate v1 manifests to a future schema version.
- **Telemetry export** to OpenTelemetry / `tracing-opentelemetry` for production observability.

---

## Documentation Plan

| Doc | Update |
|---|---|
| `docs/cli-reference.md` | New `pas init` section; updated `pas run` section noting the missing-manifest warning |
| `AGENTS.md` | Add `quality` handler reference; document the `pas init` flow alongside existing `pas plan`/`pas generate`/`pas decompose` |
| `docs/guide.md` | Add a "Quality Loops" section showing the canonical `codergen → quality` pipeline pattern |
| `docs/task-verification.md` | Cross-reference the new handler and the loop control parameters |
| `README.md` | Add `pas init` to the quick-start example |
| `SPEC_PAS_INIT.md` | Revised per Phase 0; treated as the source of truth |

---

## Beads Epic + Sub-Task Decomposition

**Epic:** `feat: pas init + quality handler (SPEC-005)` (priority P2)

> **Note:** Project memory previously indicated a transition from Beads to Linear. Per Scott's explicit instruction during planning, this epic and its children are being created in Beads. If the migration is later reversed in memory, no action needed.

### Child Issues (10)

| # | Title | Type | Priority | Depends on | Effort |
|---|---|---|---|---|---|
| 1 | `chore: revise SPEC-005 per review` | chore | P1 | — | Small |
| 2 | `feat: attractor-quality crate — manifest schema + walk-up resolution` | feature | P2 | #1 | Small |
| 3 | `feat: pas init — toolchain detection + template emission` | feature | P2 | #2 | Medium |
| 4 | `feat: pas init — TUI confirmation + non-interactive mode` | feature | P2 | #3 | Medium |
| 5 | `feat: pas init — optional LLM enrichment for polyglot repos` | feature | P3 | #3 | Medium |
| 6 | `feat: trust model — XDG trust file + prompt + auto-trust on init` | feature | P1 | #2, #3 | Medium |
| 7 | `feat: quality handler — sequential stage execution + telemetry` | feature | P2 | #2, #6 | Large |
| 8 | `feat: quality handler — loop control + checkpoint integration` | feature | P2 | #7 | Large |
| 9 | `feat: validation rule — warn at pas run when quality+manifest missing` | feature | P1 | #2 | Small |
| 10 | `docs+test: cli-reference, AGENTS.md, end-to-end integration tests` | task | P2 | #3-#9 | Medium |

### Dependency Graph

```
#1 (SPEC revision, P1)
  └── #2 (schema + resolution)
       ├── #3 (init detection)
       │    ├── #4 (TUI + non-interactive)
       │    ├── #5 (LLM enrichment)
       │    └── #6 (trust model)
       │         └── #7 (quality handler)
       │              └── #8 (loop control)
       │                   └── #10 (docs + e2e)
       └── #9 (validation rule, P1 — Scott's concern)
            └── #10
```

Parallelizable: #4, #5, #9 once their deps land. #6 can begin design work in parallel with #4/#5 (the trust file format is independent of the init TUI).

### Beads Command Plan (to run after approval)

```bash
EPIC_ID=$(bd create --type=feature --priority=2 \
  --title="feat: pas init + quality handler (SPEC-005)" \
  --description="Implement SPEC-005 per the revised spec. See docs/plans/2026-05-29-feat-pas-init-quality-handler-plan.md for the full plan including phase breakdown, system-wide impact analysis, and acceptance criteria. Locked decisions: stages-array schema, direnv-style trust prompt, handler name 'quality' (lowercase). All ten child issues are decomposed in the plan." \
  | grep -oE 'beads-[a-z0-9]+')

# Then 10 children with dependency edges:
# #1 → #2 → {#3, #9} → {#4, #5, #6 (← also depends on #3)} → #7 → #8 → #10
# (commands generated and executed in the create-epic turn)
```

---

## Sources & References

### Origin

- **Review document:** [`docs/reviews/SPEC_PAS_INIT-review.md`](../reviews/SPEC_PAS_INIT-review.md) — full P1/P2/P3 finding list. Key carried-forward decisions: missing-manifest behavior (P1-1), trust model (P1-2), telemetry contract (P1-3), schema restructure (P1-4), handler naming (P2-6).
- **SPEC:** `/Users/scott/Downloads/SPEC_PAS_INIT.md` (SPEC-005) — to be revised in Phase 0 and committed into `docs/specs/` as part of #1.

### Internal References

- `crates/attractor-cli/src/main.rs:13-197` — `Cli` and `Commands` enum (new `Init` variant to add)
- `crates/attractor-pipeline/src/handlers/manager.rs` — handler registration target
- `crates/attractor-pipeline/src/handlers/codergen_handler.rs` — reference implementation for a tokio-based handler with process spawning
- `crates/attractor-pipeline/src/checkpoint.rs` — checkpoint schema (Phase 6 extends this)
- `crates/attractor-pipeline/src/validation.rs` — validation rule registry (Phase 7 adds rule here)
- `docs/cli-reference.md` — current subcommand reference style (template for `pas init` section)
- `AGENTS.md` — current agent-facing instructions (template for `quality` handler section)
- `templates/pas.md` — existing pipeline pattern documentation

### External References

- [`toml` crate (1.x)](https://docs.rs/toml/latest/toml/) — manifest parser
- [`toml_edit` crate](https://docs.rs/toml_edit/latest/toml_edit/) — comment-preserving manifest writer for `pas init`
- [`dialoguer`](https://docs.rs/dialoguer/) — TUI confirmation
- [`directories`](https://docs.rs/directories/) — XDG path resolution
- [direnv `allow` UX](https://direnv.net/docs/legacy.html#allow) — trust-prompt UX reference
- [VS Code Workspace Trust](https://code.visualstudio.com/docs/editor/workspace-trust) — alternative trust UX for reference
- [RFC 8259 — JSON](https://datatracker.ietf.org/doc/html/rfc8259) — telemetry contract reference

### Related Work

- Existing CLI subcommands following the same `cmd_*` pattern: `cmd_plan`, `cmd_decompose`, `cmd_scaffold`, `cmd_generate`, `cmd_launch` (all in `crates/attractor-cli/src/commands/`).
- Existing handler implementations: `codergen_handler.rs`, `wait_human.rs`, `parallel.rs`.
- Existing timeout tier table in `docs/cli-reference.md` ("Timeout tiers" — Trivial/Light/Standard/Heavy/Intensive) — `[quality.hooks.<stage>.timeout_secs]` should align to these tiers as sensible defaults.
