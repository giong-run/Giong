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
   - checks that the site still builds; it takes the version from `Cargo.toml` and the
     release notes from `CHANGELOG.md` when it deploys.

   From a checkout instead: `python3 scripts/release.py prepare auto --pr` (needs `gh`).

2. **Polish it.** Push to the release branch or edit on GitHub until the `CHANGELOG.md`
   section reads well for users: one line on what the release is about, then the
   entries under *Added*, *Changed*, *Deprecated*, *Removed*, *Fixed*, *Security*. Mark
   breaking changes **Breaking:** and say how to migrate. The release pull request of
   1.0.0-rc.1 also removed every `#[deprecated]` item, listing each under *Removed*
   with the name that replaces it, and gave `giong::sync::mpsc::Sender` and `Receiver`
   to the bounded channel; no alias was kept (STABILITY.md, *Changes made before 1.0*).
   From 1.0.0 on a deprecated item stays for the rest of 1.x. Thank outside contributors in
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

## Release candidates of 1.0

Every `1.0.0-rc.N` says which soaked code it stands on and what changed since. The release
pull request carries this checklist, filled in, in its description:

1. **The soaked commit**: its hash and tree, the machines, both drivers' start and end
   times, and the results directory under `bench/results/` (for 0.9: tree `f1b23074`,
   commits `674c42c` and `003d9e5`, PR #155).
2. **The release commit**: the hash the release pull request is based on.
3. **What changed between them**: `git diff --stat <soaked>..<release> -- src giong-*/src`
   pasted in, and each pull request in that range listed with one of:
   - *soak path*: it changes `src/driver/`, `src/op.rs`, `src/buf.rs`, `src/kbox.rs`,
     `src/slab.rs`, `src/runtime.rs`, `src/executor.rs`, `src/time.rs`, `src/net.rs`,
     `src/net/`, `src/sync/`, `src/fs.rs`, or the connection, HTTP/2 or TLS state of
     `giong-http`'s server and of `giong-tls`, in lines that run (doc comments do not
     count);
   - *outside*: everything else (the database crates, `giong-http`'s client, `giong-web`,
     `giong-mcp`, docs, benches, tests).
4. **The tests run again on the release commit**, each named with its run: the full CI
   set (kernels 5.15 to 6.18, both drivers, TSan, loom, Kani, the simulation soak), the
   cancellation tests of STABILITY.md's *Cancellation* section on both drivers, and for
   each *soak path* pull request the tests it added or that cover the code it changed.
5. **The soak, sized by what changed** (see [How long to soak](#how-long-to-soak)): the
   tier the range falls in, the commit that was soaked, for how long on each driver, and
   the results directory. For 1.0.0-rc.1: the 72-hour soak of 0.9 stands for the code it
   covered, and the soak-path changes since tree `f1b23074` (#134's bind, the channel, UDP,
   close and TLS receive fixes) get a 4-hour soak per driver on `main` once they have all merged.
6. **The removals**: every item of STABILITY.md's *Changes made before 1.0* checked off.

### How long to soak

A soak costs machine time and holds merges back, so its length follows what changed since
the last soaked commit, not the calendar. The longest tier any pull request in the range
falls in decides:

| What changed | Soak on bare metal, per driver |
|---|---|
| Nothing on a soak path: docs, benches, tests, the database crates, `giong-web`, `giong-mcp`, the HTTP client | none |
| A soak path (item 3 above): the drivers, the scheduler, a cancellation path, sockets, channels, timers, files, or the connection, HTTP/2 or TLS state of the servers | 2 to 4 hours |
| A new major version, a new driver (kqueue, IOCP) or a replaced scheduler | 72 hours |

A short soak runs the same `tests/soak.rs` and the same checks as the long one (no leak of
memory or descriptors, no crash, no stall), and `giong-tls/tests/soak.rs` when the range
touches TLS (`scripts/check.sh soak tls`; with the kernel's `tls` module loaded, half its
connections use kernel TLS); two hours for a change the tests of item 4
cover line by line, four when it reaches further. The 72-hour soak also runs once on a 1.x
release when none has run for six months, so slow leaks cannot build up unseen.

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
Release, so it publishes nothing. A green `publish` job proves that the token exchange
works, which needs at least one crate whose trusted publisher matches; check each crate's
settings page to be sure every one is in place.

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
   If the new crate depends on other workspace crates at a version that is not on
   crates.io yet (it is released together with them), publish those first, by hand too,
   from the release tag. After the release pull request is merged, `release.yml` stops
   at this check with nothing published; check out `vX.Y.Z` and publish the new crate
   with every workspace crate it depends on in one command, for example
   `cargo publish -p giong -p giong-tls -p giong-http -p giong-web` (Cargo 1.90 or newer
   puts them in dependency order). Then add the trusted publisher and re-run the
   workflow: it skips what is already published. Dev-dependencies count too: giong-compat's
   tests use giong-http, so 0.5.0 published giong, giong-tls, giong-http and giong-compat.
3. **Add its trusted publisher** at `https://crates.io/crates/<new-crate>/settings`:
   *Trusted Publishing → Add → GitHub*, owner `giong-run`, repository `Giong`, workflow
   `release.yml`, environment `release`.
4. It appears on the site by itself with its `Cargo.toml` description; give it an entry in
   `site/content.py` (`CRATES`) for its place in the stack, bullets and a headline fact.

From the next release on it is published with the others.

## One-time setup

Done once per repository; listed so it can be checked or rebuilt.

| What | Where | State |
|---|---|---|
| Trusted publisher for each crate: `giong-run` / `Giong` / `release.yml` / environment `release` | crates.io → crate → Settings | giong, giong-tls, giong-http, giong-postgres, giong-mysql, giong-redis: done (2026-11-13); giong-web, giong-mcp (0.4.0) and giong-compat (0.5.0): first version published by hand, then added (see [Adding a crate](#adding-a-crate)). All nine crates done (2026-11-14) |
| Environment `release`, deployments from `v*` tags only | repo Settings → Environments | done |
| Branch protection on `main` with the CI checks `lint`, `msrv`, `test`, `fallback`, `tsan` | repo Settings → Branches | done |
| Merge queue on `main`: *Require merge queue* (squash, build concurrency 2), and `all-checks` added to the required checks, so that every CI job holds a merge back | repo Settings → Branches → `main` | done (2026-11-15) |
| *Allow GitHub Actions to create and approve pull requests* | org Settings → Actions → General, then repo Settings → Actions → General | needed for Prepare release |
| `SITE_DEPLOY_KEY`: a deploy key with write access on giong-run.github.io, its private half as a secret here | giong-run.github.io → Settings → Deploy keys; Giong → Settings → Secrets → Actions | needed for automatic site deploys |

Without the last two the process still works with a little more by hand: run
`scripts/release.py prepare --pr` locally instead of the workflow, and after a release run
`python3 site/build.py` and copy `site/public/` to giong-run.github.io.

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
- **giong.run follows releases by itself.** Its pages are built when it deploys, from
  `Cargo.toml`, `CHANGELOG.md` and `ROADMAP.md`; CI fails if the site does not build or
  has a broken link, and the release deploys it.
- **Nothing names the crates by hand,** so a new crate cannot be left out of a release.
- **The publish job runs only pinned code.** Every action in every workflow is pinned to a
  full commit SHA (its version in a comment; Dependabot proposes updates once a month),
  because any step of the `publish` job could ask for the OIDC token and publish. Its
  checkouts keep no credentials, and it runs `cargo publish --no-verify`: the `verify` job
  has already built every crate from its package without any secret, so no dependency's
  build script or proc macro runs while the publish token exists.
