# Releasing

Releases are annotated semver git tags (`vX.Y.Z`) on `main`, published to
crates.io from the tagged tree. Projects depend on
`trestle-framework = "0.8"` (or pin a git tag with
`trestle = { package = "trestle-framework", git = "…/trestle", tag = "vX.Y.Z" }`).

1. Merge the release's changes to `main`. CI must be green. While the CI
   workflow is disabled, run its jobs locally on the release branch instead
   (see [Checking locally](#checking-locally)).
2. On a release branch:
   - set `[workspace.package] version` in `Cargo.toml` to `X.Y.Z`, and the
     internal `version = "=X.Y.Z"` pins in `[workspace.dependencies]`
   - in `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`,
     add a new empty `## [Unreleased]` above it, and update the links at
     the bottom
   - merge the branch
3. Tag the merge commit and push the tag:
   `git tag -a vX.Y.Z -m "Trestle X.Y.Z" && git push origin vX.Y.Z`.
4. CI runs on the tag. `scripts/ci/release-check.sh` fails the run if the
   tag, the workspace version and the changelog disagree. From then on, the
   upgrade job migrates from this tag on every pull request. While CI is
   disabled, run `scripts/ci/release-check.sh vX.Y.Z` yourself before
   tagging.

5. From the tagged tree, `cargo publish -p <crate>` (dry-run first) in
   dependency order: trestle-protocol, trestle-historical, trestle-core,
   trestle-identity, trestle-mail, trestle-framework, trestle-cli. crates.io
   rate-limits new crates; wait and retry on 429. Never yank to fix a
   mistake; publish a patch release.

`trestle new` defaults to the crates.io release of `trestle-framework`;
`--git URL --tag TAG` pins a git tag instead.

## Checking locally

The CI workflow (`.github/workflows/ci.yml`) is disabled for now. Run the
same jobs on the branch, against a local PostgreSQL
(`TRESTLE_TEST_DATABASE_URL`, default `postgres://localhost/postgres`), and
say in the pull request which ones you ran:

- `cargo fmt --all --check` and
  `cargo clippy --workspace --all-targets -- -D warnings`;
- `cargo test --workspace`;
- `cargo run -q -p pilot -- makemigrations --check`,
  `scripts/ci/replay.sh` and `scripts/ci/upgrade-from-tag.sh`;
- `scripts/ci/scaffold.sh` and
  `cargo deny check advisories bans sources licenses`;
- in `frontend/`: `npm ci`, `npm audit --omit=dev`,
  `npm audit --audit-level=high`, `npm test`, `npm run typecheck`,
  `npm run build` and `npm run test:packages`;
- for a release, `scripts/ci/release-check.sh vX.Y.Z` and
  `cargo publish --dry-run --locked` with every crate in the order above.

CI also runs the suite on PostgreSQL 13 and 17, and the browser
qualification (`views_live` with `TRESTLE_VIEW_BROWSER=1`); a local run
covers one PostgreSQL version unless you run it against others.
