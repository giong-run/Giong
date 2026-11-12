# giong.run

The project's website: one static page in English.
No framework, no build tool, no external requests: fonts and logos are served from this
folder.

```
site/
  build.py        generates public/index.html, 404.html, sitemap.xml and robots.txt
  public/         the folder to deploy
    styles.css    every style, light and dark from prefers-color-scheme
    main.js       copy button, code tabs, the event-loop animation (optional)
    fonts/        Inter and JetBrains Mono (SIL Open Font License), latin subsets
    assets/       the mark, favicon and social preview, copied from ../assets/
    CNAME         giong.run, for GitHub Pages
```

## Changing the text or a number

Edit `build.py` (the `CHARTS`, `CODE`, `COMPARE`, `ROADMAP`, `EXAMPLES`, `FAQ` and `T`
tables), then run

```bash
python3 site/build.py
```

Python 3.8+ with nothing installed. Commit the regenerated HTML with the change.

Every number on the page comes from the README of release 0.2 and `bench/results/`; the
comparison table comes from the README too, and the roadmap from `ROADMAP.md`. When a
benchmark is run again, update the README first and then the page, so the two never
disagree. Keep the caveats next to the numbers they qualify.

## Version and crates

The version on the page and the list of crates come from the workspace's `Cargo.toml`
files, so nothing here changes for a release: `scripts/release.py prepare` rebuilds the
page as part of the release pull request. `ON_CRATES_IO` at the top of `build.py` switches
the install commands between crates.io and a git tag; it has been `True` since 0.2.0.

## Looking at it locally

```bash
python3 -m http.server -d site/public 8000    # then open http://localhost:8000
```

## Deploying

giong.run is served by GitHub Pages from the
[giong-run/giong-run.github.io](https://github.com/giong-run/giong-run.github.io)
repository, which holds a copy of `site/public/`. The
[Site workflow](../.github/workflows/site.yml) copies it there (keeping that repository's
`README.md` and `.nojekyll`) whenever `site/` changes on `main`, and at the end of every
release. It holds back a version that has no published GitHub Release yet, so the page
never announces a release before crates.io has it. Pages rebuilds within a minute.

The workflow pushes with a deploy key: the `SITE_DEPLOY_KEY` secret in this repository is
the private half of a key with write access on giong-run.github.io. Without it the workflow
only checks the page. To deploy by hand instead, copy `site/public/` over the files in that
repository and push.

The domain's DNS is on Cloudflare, set to "DNS only" so GitHub can issue the certificate:
four `A` and four `AAAA` records for the apex pointing at GitHub Pages, `www` as a `CNAME`
to `giong-run.github.io`, and the `_github-pages-challenge-giong-run` TXT record that
verifies the domain for the organisation. HTTPS is enforced.

The links to `bench/*.sh`, `ROADMAP.md`, `CHANGELOG.md` and `examples/` point at the `main`
branch on GitHub.
