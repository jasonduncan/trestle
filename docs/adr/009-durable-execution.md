# ADR-009: Durable execution (jobs and outbox)

**Status:** Deferred (M5, 2026-09-26): not introduced · **Design doc:** §6.3,
§15.1–§15.2, §18 M5

## Context

M5 adds transactional jobs or an outbox only if the pilot needs durable
side effects, and only once their crash and retry tests pass (§18).

## Decision

Build nothing for the pilot. Every pilot operation's effects are rows in
its own transaction: the lesson, unit or membership plus its audit event.
No operation sends email, calls another service or must finish after the
response. `tokio::spawn` is not used for application work anywhere in the
pilot.

## Consequences

- There is no job table, worker or outbox, and no claim to at-least-once
  delivery.
- The first feature that needs an external effect (notifications, exports
  to another system) must reopen this ADR. It then needs: transactional
  enqueue in the operation's unit of work; claim tokens that fence
  completion; bounded drain on shutdown; authority re-evaluated at execution
  time; and the crash, duplicate and lease tests listed in §15.2 and §19.1,
  before any use.
- Until then, adding a detached task that users rely on is a review
  finding, not an implementation detail.
