# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's **Report a
vulnerability** button (private security advisories) on this repository. Do
not open a public issue. Include the affected version or commit, a
description, and steps to reproduce if you have them.

You should get an acknowledgement within a few days. Fixes are released as
a new version and noted in `CHANGELOG.md`.

## Supported versions

Trestle is pre-1.0. Only the latest release gets security fixes.

## Scope and known limitations

Trestle has not had an independent security review. The design's security
invariants are listed in `CONTRIBUTING.md`.

- **Credentials an application issues itself.** `password::sign_in` checks a
  password for clients that hold their own credential, such as a native
  app's token. Whatever credential the application issues afterwards lives
  outside Trestle, so it can briefly outlast a later disable of the account.
  The issuing application must handle that, for example by rechecking the
  account or by keeping those credentials short-lived. Disabling an account
  does revoke its Trestle sessions, outstanding links and API keys, in the
  same transaction that disables it.
- **Rate limits are per process and in memory.** Run edge limits as well.
  In production, set `TRESTLE_CLIENT_ADDRESS_HEADER` only behind an edge
  that overwrites the header, and make sure the API is reachable only
  through that edge.
- **Development identities** (`TRESTLE_DEV_TOKENS`) are refused in
  production. Never deploy with them.
