---
title: "feat: Claude CLI settings isolation for codergen"
type: feat
status: active
date: 2026-07-17
issue: attractor-9t9
scope: crates/attractor-pipeline/src/handlers/codergen_provider.rs Claude branch; pas run/launch CLI config plumbing; pas.toml manifest schema
---

# Claude CLI Settings Isolation for `codergen`

## Problem

PAS currently invokes Claude Code for `codergen` nodes through `claude -p` in `crates/attractor-pipeline/src/handlers/codergen_provider.rs`. The Claude branch already pins JSON output, disables session persistence, bypasses permissions for pipeline execution, enforces strict MCP config, and disables slash commands/skills. It still inherits operator user-scope Claude Code settings unless the invocation explicitly says otherwise.

That means a pipeline run can vary by machine based on the operator's personal Claude Code configuration: hooks, globally installed plugins/subagents, global CLAUDE.md discovery, effort/model defaults, plugin sync, auto-memory, and other startup customizations. That is wrong for PAS. Pipeline runs should be reproducible by default.

Scott's confirmed requirement: PAS must not inherit local/user Claude Code hooks or settings unless the user explicitly opts into that. Opt-in must be possible through both `pas.toml` and CLI flags.

## Current Code Facts

- `build_cli_command` builds the Claude invocation at `crates/attractor-pipeline/src/handlers/codergen_provider.rs:173`.
- Current Claude args include:
  - `-p <prompt>`
  - `--output-format json`
  - `--no-session-persistence`
  - `--dangerously-skip-permissions`
  - `--strict-mcp-config`
  - `--disable-slash-commands`
  - optional `--model`, `--allowedTools`, `--max-budget-usd`
- `cmd_run` constructs context in `crates/attractor-cli/src/commands/run.rs:33`; current run flags are in `crates/attractor-cli/src/main.rs:39` and do not expose Claude settings isolation yet.
- `pas.toml` exists through `attractor-quality`; current manifest schema is minimal in `crates/attractor-quality/src/manifest.rs:5` and has `[project]`, `[toolchain]`, and `[quality]` only.
- `pas generate` already has a one-off Claude hardening precedent at `crates/attractor-cli/src/commands/generate.rs:100`: it passes `--settings '{"enabledPlugins":{}}'`, `--strict-mcp-config '{}'`, and `--tools ''`, but it does not solve hooks if user scope is still loaded.

## Claude CLI Research

`claude --help` on the installed CLI exposes these relevant flags:

- `--setting-sources <sources>`: comma-separated setting sources to load: `user`, `project`, `local`.
- `--settings <file-or-json>`: path to a settings JSON file or JSON string to load additional settings from.
- `--agents <json>`: session custom agents.
- `--tools <tools...>`: built-in tool surface; `""` disables all built-ins, `default` enables default built-ins, or list explicit tools.
- `--plugin-dir <path>`: load plugin directory/zip for the session only; repeatable.
- `--strict-mcp-config`: only use MCP servers from `--mcp-config`, ignoring other MCP config.
- `--disable-slash-commands`: disable all skills.
- `--safe-mode`: disables customizations including CLAUDE.md, skills, plugins, hooks, MCP servers, commands, agents, output styles, workflows, themes, keybindings.
- `--bare`: minimal mode. The help explicitly says it skips hooks, LSP, plugin sync, attribution, auto-memory, background prefetches, keychain reads, and CLAUDE.md auto-discovery. It still allows explicit context/config via `--system-prompt`, `--add-dir`, `--mcp-config`, `--settings`, `--agents`, and `--plugin-dir`.

### Empirical Hook Test

A temporary HOME was created with a user-scope `~/.claude/settings.json` containing a `SessionStart` hook that appends `hook-fired` to a temp file. Then `claude -p 'Reply exactly OK' --output-format json --no-session-persistence --settings '{"enabledPlugins":{}}' --tools '' --strict-mcp-config '{}' --max-budget-usd 0.01` was run in four variants.

Observed result:

| Invocation | User hook fired? | Meaning |
|---|---:|---|
| `--settings '{"enabledPlugins":{}}'` only | yes | `--settings` does not suppress user-scope hooks. It is additive/overlay, not isolation. |
| `--settings ... --setting-sources project,local` | no | Excluding user source suppresses user hooks. |
| `--bare --settings ...` | no | Bare mode suppresses hooks while still accepting explicit PAS-owned settings arguments. |
| `--safe-mode --settings ...` | no | Safe mode suppresses hooks while preserving normal Claude auth; this is the practical subscription-compatible isolation primitive. |

The commands returned JSON-shaped Claude output even though the temp HOME lacked normal auth; for this research the hook side effect was the relevant pre-auth behavior.

## Decision

Use a PAS-owned `subscription_bare` mode as the default Claude Code execution mode, then layer PAS-owned explicit config on top.

Do not default to `--setting-sources project,local`. That excludes user scope, but it still says project/local settings are normal sources. Scott's workflow keeps Claude skills/plugins globally installed, and PAS should not push users toward project/local Claude Code configuration as their normal pattern.

Do not rely on `--settings` alone. It does not suppress user hooks. That fails the hard requirement.

Do not use Claude's literal `--bare` as the normal default. It suppresses the right ambient behavior, but Claude's help says `--bare` only uses `ANTHROPIC_API_KEY` or an `apiKeyHelper` supplied through `--settings`; OAuth/keychain subscription auth is not read. PAS must not force subscription users onto API-key billing.

Use `--safe-mode` for the default PAS `subscription_bare` mode. It keeps normal Claude subscription auth while disabling most customizations: CLAUDE.md, skills, plugins, hooks, MCP servers, custom commands, agents, output styles, workflows, themes, and keybindings. This is not as clean as literal `--bare`, but it is the practical default for subscription users.

Recommended default Claude branch shape for subscription-compatible isolation:

```text
claude --safe-mode -p <prompt> \
  --output-format json \
  --no-session-persistence \
  --dangerously-skip-permissions \
  --strict-mcp-config '{}' \
  --disable-slash-commands \
  --settings <PAS-owned JSON/file when configured> \
  --tools <PAS-owned built-in tool list> \
  --agents <PAS-owned agents JSON when configured> \
  --plugin-dir <PAS-owned plugin dirs when configured>
```

Recommended strict-bare shape for users who explicitly choose API-key/auth-helper mode:

```text
claude --bare -p <prompt> \
  --output-format json \
  --no-session-persistence \
  --dangerously-skip-permissions \
  --strict-mcp-config '{}' \
  --disable-slash-commands \
  --settings <PAS-owned JSON/file when configured> \
  --tools <PAS-owned built-in tool list> \
  --agents <PAS-owned agents JSON when configured> \
  --plugin-dir <PAS-owned plugin dirs when configured>
```

Keep `--strict-mcp-config` and `--disable-slash-commands` as-is. They are orthogonal and already verified.

## Desired Semantics

Default (`subscription_bare`):

- User-scope Claude Code settings are not loaded.
- User hooks do not fire.
- User/global CLAUDE.md auto-discovery does not happen.
- User/global plugin sync does not run.
- Built-in tools are explicitly controlled by PAS where Claude allows explicit override.
- MCP is empty unless PAS supplies explicit MCP config.
- Skills/slash commands remain disabled unless PAS later intentionally changes that.
- Normal Claude subscription auth still works.

Strict bare (`strict_bare`):

- Uses Claude's literal `--bare`.
- Maximizes reproducibility.
- Requires API-key/auth-helper-compatible Claude auth; it does not use normal subscription OAuth/keychain auth.

Explicit inheritance opt-in:

- CLI flags can opt into inheriting Claude settings sources for a run.
- `pas.toml` can opt into inheriting Claude settings sources for a repo/pipeline.
- CLI flags override `pas.toml` for the current run.
- Opting into inheritance should be conspicuous in logs/dry-run output.

## Proposed Config Surface

Add a `codergen`/Claude config section to `pas.toml`. Suggested schema:

```toml
[codergen.claude]
# Default: "subscription_bare". Uses --safe-mode and PAS-owned explicit config
# so Claude subscription auth still works.
# "strict_bare" uses Claude's literal --bare and requires API-key/auth-helper auth.
# "inherit" loads configured setting_sources and does not pass --safe-mode/--bare.
settings_mode = "subscription_bare"

# Only honored when settings_mode = "inherit".
# Default for inherit should require explicit values, not silently fall back to user.
setting_sources = ["user"]

# Optional PAS-owned explicit config. These work in subscription_bare and strict_bare mode.
settings = { enabledPlugins = {} }
tools = []
agents = {}
plugin_dirs = []
mcp_config = {}
```

For a smaller v1, use only:

```toml
[codergen.claude]
settings_mode = "subscription_bare" # subscription_bare | strict_bare | inherit
setting_sources = []                 # only used by inherit
tools = []
plugin_dirs = []
settings_json = "{}"
agents_json = "{}"
mcp_config_json = "{}"
```

The strongly typed TOML form is nicer long-term, but JSON string fields reduce schema work and map directly to Claude CLI args. My take: ship JSON string fields first, then add typed sugar later if real users hate it.

Add CLI flags to `pas run` and `pas launch`:

```text
--codergen-claude-settings-mode subscription-bare|strict-bare|inherit
--codergen-claude-setting-sources user,project,local
--codergen-claude-settings <file-or-json>
--codergen-claude-tools <tools>
--codergen-claude-agents <json>
--codergen-claude-plugin-dir <path>        # repeatable
--codergen-claude-mcp-config <file-or-json>
```

A shorter alias set is acceptable, but avoid vague names like `--claude-settings`; these flags affect only `codergen`'s Claude provider branch, not the whole PAS CLI.

## Precedence

1. Node/DOT attributes, where already supported for node-local behavior (`allowed_tools`, `max_budget_usd`, `llm_model`).
2. CLI flags for this run.
3. `pas.toml` `[codergen.claude]`.
4. PAS defaults.

Do not read operator Claude Code config as an implicit fallback.

## Implementation Plan

### Phase 1 — Model the config

Files:

- `crates/attractor-quality/src/manifest.rs`
- `crates/attractor-quality/src/resolution.rs` if helper resolution is added
- tests beside manifest/resolution

Tasks:

- Add optional `codergen` section to `Manifest`.
- Add `ClaudeCodergenConfig` with:
  - `settings_mode: Option<ClaudeSettingsMode>`
  - `setting_sources: Option<Vec<String>>`
  - `settings_json: Option<String>` or typed settings value
  - `tools: Option<String>`
  - `agents_json: Option<String>`
  - `plugin_dirs: Vec<PathBuf>`
  - `mcp_config_json: Option<String>`
- Validate `settings_mode` is `subscription_bare`, `strict_bare`, or `inherit`.
- Validate `setting_sources` entries are only `user`, `project`, `local`.
- Validate JSON-string fields parse as JSON if using JSON strings.

Acceptance:

- Manifest tests cover missing section, subscription-bare default, strict-bare mode, inherit with user source, invalid mode, invalid setting source, invalid JSON.

### Phase 2 — Plumb CLI options into run context

Files:

- `crates/attractor-cli/src/main.rs`
- `crates/attractor-cli/src/commands/run.rs`
- `crates/attractor-cli/src/commands/launch.rs` if launch directly forwards run options

Tasks:

- Add the CLI flags above to `Run` and `Launch`.
- Put the resolved CLI override values into `Context` before pipeline execution.
- Use stable context keys, e.g.:
  - `codergen.claude.settings_mode`
  - `codergen.claude.setting_sources`
  - `codergen.claude.settings`
  - `codergen.claude.tools`
  - `codergen.claude.agents`
  - `codergen.claude.plugin_dirs`
  - `codergen.claude.mcp_config`

Acceptance:

- CLI parser tests or focused command tests prove flags parse and populate context.
- `pas run --dry-run` output warns when inheritance is enabled.

### Phase 3 — Resolve effective Claude config for `codergen`

Files:

- `crates/attractor-pipeline/src/handlers/codergen_provider.rs`
- `crates/attractor-pipeline/src/handlers/codergen_handler.rs`
- possible helper module in `attractor-pipeline` if command builder should stay slim

Tasks:

- Extend `CliRunConfig` with effective Claude settings fields or a `ClaudeCliIsolationConfig` struct.
- Resolve config in `codergen_handler.rs` from context + `pas.toml` + defaults.
- Default to `subscription_bare` mode.
- In `subscription_bare` mode, pass `--safe-mode`, do not pass `--bare`, and do not pass `--setting-sources`.
- In `strict_bare` mode, pass `--bare` and do not pass `--setting-sources`.
- In `inherit` mode, do not pass `--safe-mode` or `--bare`; pass `--setting-sources <sources>` from CLI/TOML.
- Always keep `--strict-mcp-config`; pass `{}` unless explicit PAS MCP config is provided.
- Always keep `--disable-slash-commands` in v1.
- Pass explicit `--settings`, `--tools`, `--agents`, and repeatable `--plugin-dir` when configured.

Acceptance:

- Command-builder tests prove default Claude args contain `--safe-mode`, `--strict-mcp-config`, `{}`, and `--disable-slash-commands`.
- Command-builder tests prove default Claude args do not contain `--bare`.
- Command-builder tests prove default args do not contain `--setting-sources`.
- Strict-bare tests prove `--bare` is present and `--safe-mode` is absent.
- Inherit-mode tests prove `--safe-mode`/`--bare` are absent and `--setting-sources user` or configured list is present.
- Explicit curation tests prove settings/tools/agents/plugin-dir/mcp-config args are emitted correctly.

### Phase 4 — Empirical integration regression

Files:

- likely integration test under `crates/attractor-pipeline/tests/` or a small test helper skipped unless `CLAUDE_INTEGRATION=1` is set

Tasks:

- Create a temp HOME with a user-scope `SessionStart` hook that appends to a temp file.
- Run the exact command builder output in default subscription-bare mode against Claude CLI, ideally with a tiny budget and no model call dependency if possible.
- Assert the hook log does not exist.
- Run inherit mode with `setting_sources = ["user"]` and assert the hook fires.
- Run strict-bare mode separately only when API-key/auth-helper auth is available.

Acceptance:

- Hermetic integration test documents the empirical behavior discovered in this plan.
- Test is opt-in if it requires installed Claude CLI/auth, so normal `cargo test` remains reliable.

### Phase 5 — Docs and operator visibility

Files:

- `docs/cli-reference.md`
- `docs/guide.md`
- `docs/task-verification.md` if it discusses handler dispatch/security
- `docs/dot-dialect.md` only if node attrs are added

Tasks:

- Document that PAS codergen Claude runs use subscription-compatible bare-ish isolation by default.
- Document strict bare as a stronger opt-in mode for API-key/auth-helper users.
- Document how to opt into personal/user Claude Code config via CLI and `pas.toml`.
- Include a warning that inheriting user settings also inherits hooks.
- Show minimal examples:
  - reproducible default
  - opt into user hooks/settings
  - PAS-owned plugin dir/tool curation without user inheritance

Acceptance:

- Docs mention `--strict-mcp-config` and `--disable-slash-commands` remain default.
- Docs explicitly distinguish PAS-owned explicit `--settings` from inheriting user-scope settings.

## Non-goals

- Codex/Gemini provider branches.
- Reworking built-in tool safety beyond explicitly controlling Claude `--tools`.
- Enabling skills/slash commands by default.
- Designing a broad global PAS configuration system.
- Removing `--dangerously-skip-permissions`; that is existing behavior and a separate security review.

## Risks / Open Questions

1. `subscription_bare` is not literal bare mode. It depends on Claude `--safe-mode` suppressing the dangerous ambient behavior while preserving normal subscription auth. That is less pure than `--bare`, but it is the right default because PAS should not force API-key billing for Claude subscription users.
2. `--safe-mode` and `--bare` are Claude CLI behaviors, not a stable PAS-owned API. Pin behavior in an opt-in integration test and document the Claude CLI version in failures.
3. JSON string config fields are ugly. Typed TOML is nicer but takes more schema validation. Favor minimal JSON fields for v1.
4. Inherit mode is intentionally dangerous. It should print/log a warning because hooks can run arbitrary commands.
5. `--disable-slash-commands` may prevent explicit plugin skills from being useful even when `--plugin-dir` is supplied. That is acceptable for v1 because the confirmed requirement is reproducibility; skills can be a separate, explicit relaxation later.

## Final Recommendation

Implement a hybrid Idea B with two PAS-owned isolation levels:

- Default: `subscription_bare` = `--safe-mode` + PAS-owned explicit settings/tools/agents/plugin dirs/MCP config; works with normal Claude subscription auth.
- Opt-in strict: `strict_bare` = Claude literal `--bare`; maximum reproducibility, API-key/auth-helper only.
- Opt-in inheritance: CLI or `pas.toml` switches to `settings_mode = "inherit"` and names exact Claude `setting_sources`.
- Never rely on `--settings` alone to isolate anything.
- Keep `--strict-mcp-config` and `--disable-slash-commands`.

This satisfies the corrected ask: PAS gets its own settings surface, the default works for Claude subscription users, personal hooks/settings are suppressed as much as Claude allows without API-key-only bare mode, and users who really want global Claude Code config can opt in loudly.
