# Development workflow and GitHub setup

## Branches and commits

* `main` is always releasable-quality: every change lands through a pull
  request with green CI.
* Feature branches: `feat/<topic>`, `fix/<topic>`, `docs/<topic>`,
  `ci/<topic>`.
* Focused commits with imperative subjects (`Add PipeWire virtual sink
  nodes`), body explaining *why*.
* Significant work is tracked in GitHub Issues and referenced from PRs.

## Repository settings to apply on GitHub

These are settings, not files, so they must be applied by a repository admin
(web UI or `gh api`):

1. **Branch protection / ruleset for `main`:** require pull requests, require
   status checks `rustfmt`, `Linux x86_64`, `Linux ARM64`, `Windows x86_64`,
   `macOS ARM64`, `MSRV (1.88)`; require linear history; block force pushes
   and deletion.
2. **Actions → General:** workflow permissions "Read repository contents";
   require approval for workflows from outside collaborators.
3. **Environments:** create `release` with required reviewers. Signing keys
   (Authenticode, Apple Developer ID, notarization credentials, minisign
   update key) are stored only as secrets of this environment.
4. **Security:** enable Dependabot alerts, secret scanning and push
   protection.

## Releases

Not implemented yet (phase 8). The rules are fixed already:

* Semantic versioning; tags `vX.Y.Z` (pre-releases `vX.Y.Z-beta.N`).
* A release is produced only by the manually dispatched release workflow
  running in the protected `release` environment — never by a push to
  `main`.
* `CHANGELOG.md` is updated in the PR that prepares the release.
* CI on `main` uploads unsigned development builds as workflow artifacts
  (14-day retention); they are not releases.
