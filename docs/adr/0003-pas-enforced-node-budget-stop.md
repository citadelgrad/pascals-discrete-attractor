# 3. PAS enforces the node budget for pi by stopping the provider process

Date: 2026-10-10
Status: Accepted

## Context

Claude takes `--max-budget-usd`, so `max_budget_usd` on a Claude node is enforced by the provider. pi has no budget flag.
Without a PAS-side stop, `max_budget_usd` on a pi node would be accepted by validation and then silently do nothing.
pi reports cost in `usage.cost.total` on each assistant `message_end` event of its JSONL stdout.
The Run Journal (ADR 0001) allows `LlmInvoked` statuses `success`, `failed` and `timeout` only.

## Decision

- PAS adds the `usage.cost.total` of each assistant `message_end` line of a pi node while it streams the stdout into the Transcript.
- When the sum is above `max_budget_usd`, PAS kills the provider's process group, keeps the Transcript up to and including the line that crossed the limit, and returns a failed outcome. A sum equal to the limit does not stop the node. The comparison is made in whole nano-dollars, so float drift cannot stop a node that spent exactly its limit.
- `message_update` lines and `agent_end` carry repeated usage and are never counted.
- The `LlmInvoked` status set stays success, failed, timeout. A budget stop is `failed`, and its message names the amount spent and the limit. If the node timeout fires first, the status is `timeout`.
- The partial cost is recorded as `<node>.cost_usd`, so it counts toward the Run total. The failed outcome is not retried.
- A `max_budget_usd` that is not a finite non-negative number on a pi node fails the node before any process starts.
- The hook lives in the provider stream reader and knows nothing about pi; the pi line parsing stays with the other pi parsing code. The engine is unchanged.

## Alternatives considered

- Kill from the engine: the engine would have to learn each provider's stream format.
- A pi extension that enforces the budget inside pi: depends on a user-loaded extension, and isolation can turn extensions off.
- Ignore the attribute on pi nodes: a silent no-op on a spending limit.
- A new `LlmInvoked` status such as `budget_exceeded`: changes the Run Journal contract that ADR 0001 fixes.

## Consequences

- One assistant message can spend past the limit, because pi reports its cost only when the message ends. The limit is a stop, not a cap.
- PAS may kill a provider process because of spend; `failed` plus the message is the only signal in the Run Journal.
- A Run can exceed its own Run budget by the stopped node's cost; the existing Run budget check then ends the Run at the next node boundary.
