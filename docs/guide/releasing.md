# Releasing

Releases are annotated semver git tags (`vX.Y.Z`) on `main`. Projects pin a
tag with `trestle = { git = "…/trestle", tag = "v0.1.0" }`. Nothing is
published to crates.io until the open-source milestone.

1. Merge the release's changes to `main`. CI must be green.
2. On a release branch:
   - set `[workspace.package] version` in `Cargo.toml` to `X.Y.Z`
   - in `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`,
     add a new empty `## [Unreleased]` above it, and update the links at
     the bottom
   - merge the branch
3. Tag the merge commit and push the tag:
   `git tag -a vX.Y.Z -m "Trestle X.Y.Z" && git push origin vX.Y.Z`.
4. CI runs on the tag. `scripts/ci/release-check.sh` fails the run if the
   tag, the workspace version and the changelog disagree. From then on, the
   upgrade job migrates from this tag on every pull request.

`trestle new` defaults to the tag that matches its own version
(`v<version>`), so tag every version the wrapper ships with.
