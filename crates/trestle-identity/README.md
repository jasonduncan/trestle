# trestle-identity

Identity: server-side browser sessions with CSRF checks, password accounts (Argon2id), OIDC sign-in, JWT bearer tokens and API keys. An API key carries its actor's full authority: scope labels are stored, not enforced, and `ApiKeyIdentity` refuses keys that have them unless `accept_unenforced_scopes()` is set. Restrict a key by issuing it to a service actor with limited roles.

Part of [Trestle](https://github.com/jasonduncan/trestle), an API-first Rust
application framework. Projects depend on the `trestle-framework` crate
(imported as `trestle`); this crate is internal and carries no stability
promise of its own.

Licensed under either of Apache License, Version 2.0 or MIT license, at your
option.
