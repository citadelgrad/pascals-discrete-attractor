# Provider stream fixtures

Stdout samples used by the codergen stream parser tests
(`src/handlers/codergen_handler_tests.rs`) and by the pre-streaming Outcome
regression tests (`src/handlers/codergen_regression_tests.rs`). Each file is exactly what PAS reads
from the provider's stdout for one Model Invocation.

| File | CLI version | Command | Origin |
|------|-------------|---------|--------|
| `claude-2.1.282.stream.jsonl` | Claude Code 2.1.282 | `claude --safe-mode -p … --output-format stream-json --verbose --no-session-persistence --model haiku` | Recorded, then sanitized |
| `codex-0.151.0.jsonl` | Codex CLI 0.151.0 | `codex exec --json --skip-git-repo-check --ephemeral …` | Recorded, then sanitized |
| `gemini-0.61.0.stream.jsonl` | Gemini CLI 0.61.0 | `gemini --output-format stream-json --approval-mode yolo …` | Constructed from the 0.61.0 source (`StreamJsonFormatter`, `convertToStreamStats`) |
| `gemini-0.61.0.json` | Gemini CLI 0.61.0 | `gemini --output-format json --approval-mode yolo …` | Constructed from the 0.61.0 source (`JsonFormatter`, `uiTelemetry` metrics) |

Sanitizing replaced session IDs, UUIDs, message and request IDs, local paths,
the thinking signature, rate-limit reset times and utilization, local
tool/plugin/skill lists, and Codex's local configuration warnings. Every other
field, including the ones PAS does not read, is kept as the CLI printed it.

The Gemini files are not recordings: no Gemini credentials were available when
they were written. Replace them with recorded output when possible.

## Observed behavior

Recorded for task U2 (skill loading). The pi rows are added by U1.

### Claude skill loading

Claude Code 2.1.294. `<plug>` is a plugin directory in the C5 layout
(`.claude-plugin/plugin.json` plus `skills/pas-sentinel/SKILL.md`); `<addA>` holds
`.claude/skills/pas-sentinel/SKILL.md`; `<addB>` holds `skills/pas-sentinel/SKILL.md`. The common
tail of every command is
`-p <prompt> --output-format stream-json --verbose --no-session-persistence --dangerously-skip-permissions --strict-mcp-config --model haiku`
and does not contain `--disable-slash-commands`. A skill counts as loaded when the model's `Skill`
tool call returned the sentinel token `PAS_SENTINEL_7f3a`, and the namespaced name appeared in the
`skills` list of the `system/init` event. The probes ran with every `CLAUDE*` and `ANTHROPIC*`
environment variable removed: an inherited `CLAUDE_CODE_SAFE_MODE=1` forces safe mode in every mode
and made a first set of probes useless.

| Mode | Claude CLI version | Exact command | plugin-dir skills load | --add-dir skills load | personal skills absent |
|------|--------------------|---------------|------------------------|-----------------------|------------------------|
| `subscription_bare` | 2.1.294 | `claude --safe-mode --plugin-dir <plug> <tail>` and `claude --safe-mode --add-dir <addA\|addB> <tail>` | no | no | yes |
| `inherit` | 2.1.294 | `claude --setting-sources user,project,local --plugin-dir <plug> <tail>` and `claude --setting-sources user,project,local --add-dir <addA\|addB> <tail>` | yes | yes (only the `<addA>` layout `.claude/skills/<name>/SKILL.md`; `<addB>` layout: no) | no (present, by user choice) |
| `strict_bare` | 2.1.294 | `claude --bare ...` | not tested: no API key | not tested: no API key | not tested: no API key |

Details:

- `subscription_bare` (`--safe-mode`): the `skills` list in `init` has 3 entries (bundled) with or
  without `--plugin-dir` / `--add-dir`. `--plugin-dir` registers the plugin (`pas-skills@inline`
  appears in `plugins`) but its skills are not loaded; `Skill pas-sentinel` and
  `Skill pas-skills:pas-sentinel` both return `Unknown skill`. Also tried without success:
  `--plugin-dir` together with `--add-dir`, `--plugin-dir` twice, `--setting-sources user,project,local`
  with `--plugin-dir`, and a workdir that holds `.claude/skills/pas-sentinel`.
  **No mechanism loads named skills in subscription_bare.** On 2026-10-10 the user accepted the
  fallback: Claude nodes in this mode keep skills off and get the `CODERGEN_SKILLS_NOT_LOADED` warning.
- `inherit`: `init` lists `pas-skills:pas-sentinel` (plugin) or `pas-sentinel` (`--add-dir`
  with the `.claude/skills` layout); `Skill pas-sentinel` works with the short name in both cases.
  About 396 personal skills (for example `ab-testing`) are present too.
- `--disable-slash-commands` removed: built-in slash commands appear in `slash_commands` (39 in
  safe mode), as the spec notes.

### Plugin manifest

Claude accepted `.claude-plugin/plugin.json` with the fields `name`, `version` and `description`:
`{"name":"pas-skills","version":"0.0.0","description":"PAS test skills"}`. The skill file lives at
`skills/<name>/SKILL.md` inside the plugin directory, with front matter `name` and `description`.
The `init` `plugins` entry is `{name: pas-skills, source: pas-skills@inline, version: 0.0.0}`. The
skill is listed as `pas-skills:pas-sentinel`; the short name `pas-sentinel` also resolves. The
minimal manifest `{"name":"pas-skills"}` was not tried.

### Codex skill roots

Codex CLI 0.160.1, `codex exec --json --yolo --skip-git-repo-check --ephemeral <prompt>` in a
scratch directory. The model reported the roots below as the parents of the skill files in its
catalog (it saw 326 skills; `ab-testing` and the sentinel skill both present). Codex warned that the
skills context budget was exceeded and removed all descriptions plus 18 skills from the list.

| Root | Source |
|------|--------|
| `~/.codex/skills` and `~/.codex/skills/.system` | observed |
| `~/.agents/skills` | observed |
| `<workdir>/.agents/skills` | observed (the sentinel copy was found) |
| `~/.codex/plugins/cache/**/skills` (installed plugins) | observed |

So a Codex node reads the user's full personal skills; nothing is isolated.

### Gemini skill roots

Gemini: not verified: not installed.

### Billed model calls

- Claude `haiku`, about 25 calls, each with a prompt of one or two lines (listing or invoking
  `pas-sentinel`); each cost at most about USD 0.005 (`total_cost_usd` in the `result` event), so
  about USD 0.1 in total. The first 15 ran with the inherited `CLAUDE_CODE_SAFE_MODE` and are not
  used in the table.
- Codex default model, 1 call (prompt: list skill count and roots); usage 21483 input tokens
  (11776 cached), 2732 output tokens.
- The `init`-only probes cut the stream after the first line and are not known to be billed; their
  cost was not observable.

### Decision for downstream tasks

- `subscription_bare`: KTD10 plugin-dir not confirmed; `--add-dir` not confirmed. No loading
  mechanism. C10: the argv does not change, and the Run gets the C8 warning (user decision 2026-10-10).
- `inherit`: KTD10 plugin-dir confirmed. `--add-dir` works only for the `.claude/skills` layout.
- `strict_bare`: not tested: no API key. `claude --help` states that `--bare` reads `--plugin-dir`, so
  C10 passes the copy root in this mode.
