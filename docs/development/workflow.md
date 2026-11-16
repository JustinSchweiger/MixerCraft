# Development workflow and GitHub setup

## Branches and commits

* `main` is always releasable-quality: every change lands through a pull
  request with green CI.
* Feature branches: `feat/<topic>`, `fix/<topic>`, `docs/<topic>`,
  `ci/<topic>`.
* Focused commits with imperative subjects (`Add PipeWire virtual sink
  nodes`), body explaining *why*.
* Significant work is tracked in GitHub Issues and referenced from PRs.

## Repository settings (applied 2026-11-16)

* **Merging:** squash or rebase only; merge commits disabled; head branches
  deleted after merge.
* **Releases:** release immutability enabled.
* **Actions:** default workflow token is read-only; fork workflows do not run;
  every action must be pinned to a full commit SHA.
* **Environment `release`:** deployments only from `v*` tags. Signing keys
  (Authenticode, Apple Developer ID, notarization, minisign update key) will
  be stored only as secrets of this environment.
* **Security:** dependency graph, Dependabot alerts and malware alerts on;
  Dependabot version updates via `.github/dependabot.yml`; cargo-deny weekly.

Not available on a free private repository (would need GitHub Pro/Team or a
public repository): enforced branch protection, required reviewers on
environments, secret scanning.

## CI minutes (private repository)

The repository is private on a free GitHub account: branch protection is
not enforced and Actions minutes are limited (macOS runners count 10×,
Windows 2×; a full CI run costs about 50 minutes). Therefore:

* CI skips changes that only touch Markdown, `docs/`, license files or
  issue/PR templates. Trigger it manually from the Actions tab if needed.
* "Require green CI before merging" is a convention, not an enforced rule.

## Releases

Not implemented yet (phase 8). The rules are fixed already:

* Semantic versioning; tags `vX.Y.Z` (pre-releases `vX.Y.Z-beta.N`).
* A release is produced only by the manually dispatched release workflow
  running in the protected `release` environment — never by a push to
  `main`.
* `CHANGELOG.md` is updated in the PR that prepares the release.
* CI on `main` uploads unsigned development builds as workflow artifacts
  (14-day retention); they are not releases.
