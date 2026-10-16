# Test fixtures

`idp-test-rsa.pem` and `idp-other-rsa.pem` are **test-only** RSA keys,
generated for this repository. They sign tokens for the mock OpenID
Connect provider in `tests/support/idp.rs` and are trusted by nothing
else. `idp-other-rsa.pem` plays a forger's key that the mock provider does
not publish. Never use them outside tests.
