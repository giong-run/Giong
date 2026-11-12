# Releasing giong

A release publishes every crate in the workspace at one shared version, to crates.io and
as a GitHub Release, and updates giong.run. A maintainer starts it and merges one pull
request; everything else is automated and safe to re-run.

```
 Actions → Prepare release ──► release PR ──► review, polish, merge
                                                     │
              release-tag.yml: tag vX.Y.Z on main ◄──┘
                                   │
              release.yml on the tag:
                verify ──► crates.io (Trusted Publishing) ──► GitHub Release ──► site.yml → giong.run
```

## Cutting a release

1. **Start it.** Open [Actions → Prepare release](https://github.com/giong-run/Giong/actions/workflows/prepare-release.yml),
   choose *Run workflow* on `main`, and leave the version at `auto` (or give `patch`,
   `minor`, `major`, or an exact version such as `0.3.0` or `0.3.0-rc.1`). It opens a pull
   request `chore(release): giong X.Y.Z` on the branch `release/X.Y.Z` that:
   - bumps `version` in every workspace crate, the path dependencies between them and
     `Cargo.lock`;
   - turns `## [Unreleased]` in `CHANGELOG.md` into `## [X.Y.Z] - <date>` and updates
     the compare links at the bottom; when `Unreleased` is empty it drafts the entries
     from the conventional commit subjects, marked with a comment saying so;
   - updates install snippets such as `giong-http = "0.2"` in the crate READMEs, and
     lists any prose that still names the old minor version (for example
     "Status: 0.2.") for you to rewrite;
   - rebuilds `site/public`, which takes the version from `Cargo.toml`.

   From a checkout instead: `python3 scripts/release.py prepare auto --pr` (needs `gh`).

2. **Polish it.** Push to the release branch or edit on GitHub until the `CHANGELOG.md`
   section reads well for users: one line on what the release is about, then the
   entries under *Added*, *Changed*, *Deprecated*, *Removed*, *Fixed*, *Security*. Mark
   breaking changes **Breaking:** and say how to migrate. Thank outside contributors in
   the entry for their change: `([#4](…/issues/4), thanks to [@name](https://github.com/name))`.
   CI runs on the branch as on any pull request.

3. **Merge it.** Squash-merge as usual. That is the last manual step:
   - `release-tag.yml` sees the new version on `main`, tags the merge commit `vX.Y.Z`
     and starts `release.yml` on the tag.
   - `release.yml` checks that the tag matches every crate version, runs
     `scripts/release.py check`, stops if any crate is not on crates.io yet (see
     [Adding a crate](#adding-a-crate)), dry-runs the publish, then publishes the crates
     one by one in dependency order through Trusted Publishing and waits until crates.io
     serves each one.
   - It creates the GitHub Release `giong X.Y.Z`: the `CHANGELOG.md` section, install
     lines with crates.io and docs.rs links, a thank-you naming every contributor, and
     GitHub's generated list of merged pull requests and first-time contributors
     (configured by [`.github/release.yml`](../.github/release.yml); release pull
     requests are left out). It is marked *Latest*, or *Pre-release* for a version with
     a suffix.
   - It starts the Site workflow, which deploys giong.run with the new version.

4. **Check it.** The run's summary shows the notes. Look at the
   [Release](https://github.com/giong-run/Giong/releases/latest), the crate pages on
   crates.io (the README, including the dark-mode logo) and https://giong.run.

## When something fails

Every step is idempotent: versions already on crates.io are skipped and an existing
Release is kept (an existing draft is filled in and published). Fix the cause on `main`
if needed and use *Re-run failed jobs* on the Release run.

- **No tag after the merge**: run [Tag release](https://github.com/giong-run/Giong/actions/workflows/release-tag.yml)
  by hand, or push the tag yourself; a tag pushed by a person starts `release.yml` directly:
  ```bash
  git tag -a vX.Y.Z -m "giong X.Y.Z" <merge-commit> && git push origin vX.Y.Z
  ```
- **The auth step fails** ("No Trusted Publishing config found"): a crate is missing its
  trusted publisher; see [One-time setup](#one-time-setup).
- **A bad version reached crates.io**: it cannot be replaced or deleted. Yank it
  (`cargo yank --version X.Y.Z <crate>`) and release a fixed patch version.

## Checking that Trusted Publishing works

Re-run the latest Release run (*Re-run all jobs*), or run the Release workflow by hand
with *Use workflow from: Tags → vX.Y.Z* (for tags made after this process was set up). It
exchanges the token for real, skips every crate as already published and keeps the
Release, so it publishes nothing. A green `publish` job means every crate's trusted
publisher is in place.

## Adding a crate

Crates are found from `[workspace] members` in the root `Cargo.toml`
(`python3 scripts/release.py crates` lists them in publishing order), so the workflows,
release notes, checks and site need no edit. A member with `publish = false` is left out.
What a new crate needs:

1. Its `Cargo.toml` uses the workspace's current version, names the other workspace
   crates it uses as `{ path = "...", version = "<that version>" }`, and has
   `description`, `license`, `repository`, `readme` and `documentation` like the others.
   Its README uses absolute image URLs (CI checks this).
2. **First publish by hand.** crates.io can only attach a trusted publisher to a crate
   that exists, so one crate owner publishes its first version from a machine logged in
   with `cargo login`:
   ```bash
   cargo publish -p <new-crate>
   ```
   Until then, `release.yml` refuses to start publishing and names the crate.
3. **Add its trusted publisher** at `https://crates.io/crates/<new-crate>/settings`:
   *Trusted Publishing → Add → GitHub*, owner `giong-run`, repository `Giong`, workflow
   `release.yml`, environment `release`.
4. Describe it on the site (cards, examples) in `site/build.py` if it deserves a mention.

From the next release on it is published with the others.

## One-time setup

Done once per repository; listed so it can be checked or rebuilt.

| What | Where | State |
|---|---|---|
| Trusted publisher for each crate: `giong-run` / `Giong` / `release.yml` / environment `release` | crates.io → crate → Settings | giong, giong-tls, giong-http: done (2026-11-12) |
| Environment `release`, deployments from `v*` tags only | repo Settings → Environments | done |
| Branch protection on `main` with the CI checks `lint`, `msrv`, `test`, `fallback`, `tsan` | repo Settings → Branches | done |
| *Allow GitHub Actions to create and approve pull requests* | org Settings → Actions → General, then repo Settings → Actions → General | needed for Prepare release |
| `SITE_DEPLOY_KEY`: a deploy key with write access on giong-run.github.io, its private half as a secret here | giong-run.github.io → Settings → Deploy keys; Giong → Settings → Secrets → Actions | needed for automatic site deploys |

Without the last two the process still works with a little more by hand: run
`scripts/release.py prepare --pr` locally instead of the workflow, and copy
`site/public/` to giong-run.github.io after a release.

## Version numbers

The crates follow [Semantic Versioning](https://semver.org). `auto` picks the bump from
the commit types since the last tag. While the version is `0.x`, a breaking change
(`feat!:` or a `BREAKING CHANGE:` footer) bumps the minor version and anything else the
patch; from `1.0` on, breaking → major, `feat` → minor, otherwise patch. Releases with
only `docs`, `ci`, `chore`, `test` or `bench` commits need an explicit version.

## Why it works this way

Lessons from 0.2.0 and 0.2.1, now enforced by the tooling:

- **README images must be absolute URLs.** crates.io rewrites a relative `<img src>`
  but not `<source srcset>`, so the dark-mode logo broke there. `scripts/release.py
  check` runs in CI and fails on any relative image and on links into this repository
  that point at files that do not exist.
- **Every release goes to both crates.io and GitHub,** with notes that read well and
  credit contributors, so neither is done by hand or forgotten.
- **giong.run follows releases by itself.** The page reads the version from `Cargo.toml`,
  CI fails if `site/public` is stale, and the release deploys it.
- **Nothing names the crates by hand,** so a new crate cannot be left out of a release.
