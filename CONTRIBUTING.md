# Contributing

- `main` is the stable/default branch, only updated from `dev` after review.
- `dev` is the integration branch for ongoing work.
- Branch features/fixes off `dev`: `git checkout dev && git checkout -b feature/whatever`.
- Open PRs **against `dev`**, not `main`.

## Before opening a PR

Run `make check test`.

This is hardware-specific code (OV02C10 sensor / Intel IPU6) that CI can't
meaningfully exercise: CI lints, runs the unit tests, and checks that the
package installs. Test your change on real hardware and note what you
checked in the PR description. Every PR's CI run has the built `.deb` as a
downloadable artifact, and every push to `dev` updates the `dev-latest`
pre-release.

## Releases

Bump `version` in `Cargo.toml`, merge to `main`, then tag `vX.Y.Z` on
`main`. CI builds the release from the tag.
