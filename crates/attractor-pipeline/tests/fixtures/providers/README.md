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
