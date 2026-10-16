# ADR-011: Outgoing email

**Status:** Accepted (2026-09-26)

## Context

Password accounts start and recover through one-time links. An operator
can print a link and hand it over, but emailing it is the normal path.
The project owner's direction was that the first step for email is simply being able
to configure SMTP, because Trestle only needs to *send*. Integrations with
email services can be added later.

## Decision

A small crate, `trestle-mail`, with one interface: a `Mailer` sends one
plain-text `Email`. The backend is chosen by URL, like Django's
`EMAIL_BACKEND`:

| `TRESTLE_EMAIL_URL` | Behaviour | Production |
|---|---|---|
| `smtps://user:pass@host[:465]` | TLS from the first byte | yes |
| `smtp://user:pass@host[:587]` | STARTTLS **required**; never falls back to plaintext | yes |
| `smtp+insecure://localhost:PORT` | plaintext to a loopback mail catcher; credentials refused | refused |
| `file:///dir` | one `.eml` per message | refused |

- **Sender and trust.** `TRESTLE_EMAIL_FROM` is the sender.
  `TRESTLE_EMAIL_CA_FILE` pins a private CA: only that CA is trusted.
  Otherwise the system trust store is used (rustls with aws-lc-rs, through
  lettre).
- **Checked up front.** Configuration is validated before anything is
  created. An operator command with a bad mail setting fails before it
  issues a link.
- **Never logged.** Message bodies carry sign-in links, so they are not
  logged.
- **Who sends.** Only operator commands send mail today:
  - `pilot account invite --send`;
  - `pilot account reset-link --send`;
  - `pilot mail test --to`, which is Django's `sendtestemail`.
- **The server sends none.** Sending from a request, for example
  self-service "forgot password", needs reliable delivery, which means the
  transactional outbox deferred in ADR-009. That is the next step if
  wanted.

## Consequences

- An email service with an HTTP API (SES, Postmark) is a new `Mailer`
  implementation; nothing else changes.
- Operators run the send. If SMTP fails, the command says the link was
  issued but not sent; running it again replaces the link.
- No HTML email, attachments or templates engine: text messages written in
  `pilot::mail`.

## Evidence

- `crates/trestle-mail/tests/smtp.rs` runs against an in-process SMTP
  server:
  - plaintext to loopback delivers;
  - STARTTLS-required against a server without STARTTLS sends neither
    credentials nor the message;
  - implicit TLS with a pinned test CA delivers and authenticates;
  - an untrusted certificate is refused before any SMTP exchange;
  - the file backend writes one message per file.
- Unit tests cover the production policy, credential decoding, secrets kept
  out of descriptions, and header-injection attempts in recipients.
- `docs/evidence/accounts-demo.json`: invites and resets sent by the CLI
  through the file backend; the link is read from the message and opened
  in Chrome.
- **Not yet done:** a send through a real provider's SMTP relay.
