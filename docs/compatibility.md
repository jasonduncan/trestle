# Compatibility matrix

What works across versions during the pilot (design doc §11.3–§11.4,
§16.3). Every row points to the fixture or demonstration that proves it.
Anything not listed is not claimed.

Terms:

- **M4 / M5 API**: the `pilot serve` binary built from tag `m4-complete` or
  from the M5 commit.
- **Bundle**: a built product or admin frontend.
- **Schema**: the migrations applied. **M5 schema** = M4 plus
  `identity.0001_initial` and the expand migration `lessons.0004_duration`.
- **Form revision**: the `r-…` revision of an interaction descriptor that
  the browser loaded.

## Replicas and schema

| API | Schema | Result | Evidence |
|---|---|---|---|
| M4 | M4 | Works (baseline) | m5-demo step 1 |
| M4 | M5 (expanded while it serves) | Works: reads and writes succeed; the new column stays NULL on its writes | m5-demo step 2 (`oldReplicaWriteAfterExpand`) |
| M5 | M4 (not yet expanded) | **Refuses to start**: required migrations `lessons.0004_duration` and `identity.0001_initial` are missing | `pilot serve` boot check; `examples/pilot-migrations` tests |
| M5 | M5 | Works | all M5 tests |

The order is fixed: **expand, then deploy**. The new binary cannot run ahead
of the schema.

## Bundles and APIs (runtime forms)

| Bundle | Form revision it submits | API that receives it | Result | Evidence |
|---|---|---|---|---|
| M4 product | M3/M4 create `r-b68fa979090594a2` | M5 | 201; `duration_minutes` stays null | `tests/additive.rs`; m5-demo step 5 (`Ratios`) |
| M4 admin | M4 edit `r-9adfca22d27dfc4e`, opened before the deploy | M5 | 200; only the changed field is written | m5-demo step 3 (`inFlightEdit`) |
| M4 admin | M4 edit, stale ETag | M5 | 412; nothing written; the reopened edit sends only its change and leaves `duration_minutes` intact | m5-demo step 4; `tests/additive.rs` |
| M5 product | M5 create (with duration) | M5 | 201 with the new field | m5-demo step 3 (`newBundleWrite`) |
| M5 product | M5 create | **M4** (skew during rollout) | **409 `interaction_stale` before any write.** The form reloads the descriptor from whichever replica answers, keeps compatible entries (title kept; duration dropped because the M4 form lacks it) and asks the person to review | m5-demo step 3 (`skew`) |
| Any bundle | reads (lists, retrieve, manifest, descriptors) | M4 or M5, round-robin | All succeed; M4 responses lack `duration_minutes`, which the M5 product shows as "—" | m5-demo step 3b (`servedBy` 15/15, 0 failures) |
| M2 release A / B bundles | `r-acbd164ff4e78da6`, `r-cab979f086c703f7` | M5 | Still accepted (retained, verified against the current contract) | `tests/published.rs` (`retained_revisions_are_recorded_and_still_compatible`) |

**M4 bundles against the M5 API get the M5 form.** The renderer is the same
code; it renders whatever descriptor the replica publishes, including the
new optional field, without a rebuild (the M2 capability). An old bundle
therefore shows the old form only while an old replica serves it.

**Retention policy.** A form revision is retained only while its
submissions mean the same thing under the current input contract, which a
test proves. Revisions published only in local demonstrations (for
example `r-ff12a6ddaa0b96ac`) are recorded in `revisions.json`, but not
retained.

## Clients without forms

| Client | Result |
|---|---|
| Generated TypeScript SDK built for M4, against M5 | Works. Inputs are a subset of M5's; M5 outputs add a nullable field that M4 types do not name. SDK types describe the server's promises and do not re-validate responses. |
| Plain HTTP clients (no revision header) | No revision check. Contract validation applies as usual; unknown fields are rejected, so a client must not send `duration_minutes` to an M4 replica. |

## Identity

| Credential | M4 API | M5 API (development) | M5 API (production) |
|---|---|---|---|
| Development bearer token | ✓ | ✓ | refused at boot |
| Session cookie from OIDC login | route does not exist (404) | ✓ if OIDC is configured | ✓ |
| Session cookie from password sign-in | route does not exist (404) | ✓ if `TRESTLE_PASSWORD_LOGIN` | ✓ if `TRESTLE_PASSWORD_LOGIN` |
| Provider JWT access token | — | ✓ if `TRESTLE_OIDC_API_AUDIENCE` is set | ✓ if set |

The first-party clients detect the mode through `GET /api/v1/auth/session`:

- 404: development tokens;
- 401: show "Sign in";
- 200: session.

## Not covered

- Contract steps (dropping or tightening columns) during a rollout.
  Contraction happens only after the compatibility window, as its own
  release.
- Long-lived tabs across two or more expand releases without a reload.
  Each retained revision is listed explicitly; nothing is retained by
  default.
- Browsers other than Chrome. The demonstrations use system Chrome in
  headless mode.
