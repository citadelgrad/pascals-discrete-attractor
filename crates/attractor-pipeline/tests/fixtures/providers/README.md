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
| `pi-1.0.4.jsonl` | pi 1.0.4 | `pi -p --mode json --no-session -ne -ns -np -nc --no-mcp -na --offline --model openai/gpt-5.5 --tools read,bash -- "Run the command: echo ok, with the bash tool. Then reply with the single word done."` with `PI_TELEMETRY=0`; exit 0 | Recorded, then sanitized |
| `pi-1.0.4.error.jsonl` | pi 1.0.4 | `pi -p --mode json --no-session -ne -ns -np -nc --no-mcp -na --offline --model openai/gpt-5-nano --tools read,bash -- "Run the command: echo ok, with the bash tool. Then reply with the single word done."` with `PI_TELEMETRY=0`; exit 0 | Recorded, then sanitized (real error: the ChatGPT-account route rejects `gpt-5-nano` with a 400) |
| `pi-1.0.4.killed.jsonl` | pi 1.0.4 | Same command as `pi-1.0.4.jsonl`; SIGTERM sent right after the first assistant `message_start`; exit 143 | Recorded, then sanitized; the file ends after that line |
| `pi-1.0.4.length.jsonl` | pi 1.0.4 | Same command as `pi-1.0.4.jsonl` | Derived from `pi-1.0.4.jsonl`: `sed 's/"stopReason":"stop"/"stopReason":"length"/g'`; the only changed field is `stopReason`, in the final assistant `message_end`, its `turn_end` and its copy inside `agent_end` (3 lines) |

Sanitizing replaced session IDs, UUIDs, message and request IDs, local paths,
the thinking signature, rate-limit reset times and utilization, local
tool/plugin/skill lists, and Codex's local configuration warnings. Every other
field, including the ones PAS does not read, is kept as the CLI printed it.

The pi files were sanitized in the same way: the session UUID and the `call_`, `fc_`, `msg_` and
`resp_` IDs became numbered placeholders (the same value maps to the same placeholder inside one
file, so tool-call references stay intact), `textSignature` became `SANITIZED`, the scratch
working directory became `/tmp/pas-fixture`, and the pi install path became `/opt/pi`. Timestamps,
`usage`, `cost`, `model`, `stopReason` and every event type are as pi printed them.

The Gemini files are not recordings: no Gemini credentials were available when
they were written. Replace them with recorded output when possible.

## Observed behavior

Recorded for task U2 (skill loading) and task U1 (pi).

### pi 1.0.4

pi 1.0.4 (`/opt/homebrew/bin/pi`), `PI_TELEMETRY=0`, run in a scratch directory outside the repo with
the C1 argv (`-p --mode json --no-session -ne -ns -np -nc --no-mcp -na --offline --model <m>
--tools read,bash -- <prompt>`). Every C1 flag was accepted by 1.0.4. `--credentials` was never
passed.

| Question | Answer | Evidence |
|----------|--------|----------|
| Q2: do compaction or auto-retry calls appear as assistant `message_end` with cost? | not observed | The runs were one or two turns; none compacted or retried. The parser still sums every assistant `message_end` (KTD5). |
| Q3: does any output list the loaded skills? | yes | With `--skill <dir>` and `-ns`, the first `message_start` has `message.role` `system` and `message.sections.skills`, a string `<skills><available_skills><skill><name>pas-sentinel</name>…<location>…/pas-sentinel/SKILL.md</location></skill>…`. It lists only `pas-sentinel`. Without `-ns`, the personal skills are listed too. The model answered `PAS_SENTINEL_7f3a`. Nothing goes to stderr. |
| Q4: does an API-key environment variable alone make `pi auth check` ready? | yes | `ANTHROPIC_API_KEY=<fake> pi auth check --provider anthropic --json` prints `{"status":"ready","provider":"anthropic","authType":"api_key"}`, exit 0. "ready" means a credential is configured, not that it is valid. |
| Q6: do the session header or events carry credentials? | no | The `session` header has only `type,version,id,timestamp,cwd`. Scans of stdout and stderr of all runs found no key, token, `Bearer`, `eyJ`, email address or account id. They do carry the absolute `cwd`, the pi install path, and the response IDs (sanitized). |

`pi auth check --json` outputs (no model call):

| Command | Output | Exit |
|---------|--------|------|
| `pi auth check --model openai/gpt-5.5 --json` (OAuth) | `{"status":"ready","provider":"openai","authType":"oauth"}` | 0 |
| `pi auth check --provider anthropic --json` (no credential) | `{"status":"not_ready","provider":"anthropic","reason":"credentials_not_configured"}` | 1 |
| `ANTHROPIC_API_KEY=<fake> pi auth check --provider anthropic --json` | `{"status":"ready","provider":"anthropic","authType":"api_key"}` | 0 |

Shape of the JSON stream, for the parser and the Monitor:

- Event `type` values: `session`, `agent_start`, `turn_start`, `message_start`, `message_update`,
  `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `turn_end`,
  `agent_end`, `agent_settled`. `agent_settled` comes after `agent_end` on a normal finish.
- `message_start`/`message_end`/`turn_end` carry `message` with `role` (`system`, `user`,
  `assistant`, `toolResult`). The `system` message has `content: ""` and `sections`
  (`preamble`, `tools`, `rules`, `docs`, `cwd`, and `skills` when skills load).
- An assistant `message_end` carries `message.usage` (`input`, `output`, `cacheRead`, `cacheWrite`,
  `totalTokens`, `cost.{input,output,cacheRead,cacheWrite,total}`), `message.model`,
  `message.provider`, `message.stopReason` (`toolUse`, `stop`, `error`), `message.errorMessage` on
  error, and `rawStopReason`. A `turn_end` repeats the same message with `toolResults`.
- The assistant `message_start` has `stopReason` `pending` and zero usage. `message_update` carries
  `assistantMessageEvent` (`text_start`, `text_delta`, `text_end`, `toolcall_start`,
  `toolcall_delta`, `toolcall_end`) and a top-level `usage` that stays zero in these runs; the
  cost is read from `message_end`.
- `agent_end.messages` repeats every message of the run (`system`, `user`, `assistant`,
  `toolResult`, `assistant`). Do not sum usage from it.
- Text is in `message.content[]` entries with `type` `text`.
- Error run: pi exited 0 and the only assistant `message_end` had `stopReason` `error`,
  `errorMessage` `OpenAI API error (400): …`, all usage zero, and `agent_end` was present. Status
  must come from `stopReason`, not the exit code (C2).
- Killed run: SIGTERM gave exit 143 and no `agent_end`.
- `usage.cost.total` is present on every assistant `message_end` of `pi-1.0.4.jsonl` (0.00461 and
  0.00425 USD), so the budget stop (C9) has data.
- On the ChatGPT-subscription OAuth route, many `openai` models fail with a 400
  (`gpt-5-nano`, `gpt-5.4-mini`, `gpt-5.4-nano`, `gpt-5.3-codex-spark`, `gpt-5.1`, `gpt-5.3-codex`,
  `gpt-5.4`); `gpt-5.5` worked.

Billed model calls: `gpt-5.5` for the completed calls: 1 "reply ok" probe, the success run, the
skill probe without `-ns`, and the skill probe with `-ns` (a first `-ns` attempt produced no output
and was stopped; its cost is not known); plus the killed run, cut after the first assistant
`message_start`. Each completed call cost under USD 0.01. Seven other model attempts (`gpt-5-nano`
twice, `gpt-5.4-mini`, `gpt-5.4-nano`, `gpt-5.3-codex-spark`, `gpt-5.1`, `gpt-5.3-codex`,
`gpt-5.4`) were rejected with zero usage.

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

## Q5 and stdin (pi 1.0.4, U4 probes, no cost)

- Q5: pi expands any message argument whose first character is `@` into
  `<file name="…">…</file>` content, even after `--` and with `--tools grep`.
  A leading space or newline, or text before the `@`, keeps it literal. PAS
  puts one space before a prompt that starts with `@` and sends the prompt as a
  single argument.
- stdin: in print mode with a non-TTY stdin, pi reads to EOF and prepends the
  data to the message. An open pipe hangs it; `/dev/null` runs at once. PAS
  starts pi with stdin closed (`Stdio::null()`).
