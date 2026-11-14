# giong.run

The project's website: six static pages in English. No framework, no build tool to
install, no external requests: fonts and logos are served from this folder.

```
site/
  build.py        generates the pages into public/ (run it, do not edit its output)
  content.py      what a person writes: the pitch, benchmark notes, code samples, FAQ
  md.py           the small Markdown renderer for ROADMAP.md and CHANGELOG.md
  art.py          the drawings that explain the design (inline SVG, coloured by styles.css)
  public/         the folder that is deployed
    styles.css    every style; dark by default, light from the system or the toggle
    main.js       theme toggle, phone menu, copy buttons, code tabs, the ring animation
    fonts/        Inter and JetBrains Mono (SIL Open Font License), latin subsets
    assets/       the mark, favicon and social preview, copied from ../assets/
    CNAME         giong.run, for GitHub Pages
```

| Page | Address | Made from |
|---|---|---|
| Home | `/` | `content.py`, the crates, the latest release |
| Docs | `/docs/` | `content.py`, the crates, `docs/errors/README.md` |
| Crates | `/crates/` | `content.py` `CRATES`, each crate's `Cargo.toml` and `src/` |
| Benchmarks | `/benchmarks/` | `content.py` `CHARTS`, `CAVEATS`, `COMPARE` |
| Roadmap | `/roadmap/` | `ROADMAP.md` |
| Releases | `/releases/` | `CHANGELOG.md` |

`llms.txt`, `llms-full.txt`, `sitemap.xml`, `robots.txt` and `404.html` are generated too.
`llms.txt` and `llms-full.txt` are for language models, so they describe the release that
`cargo add` installs, not `main`: their links point at the release tag (`blob/vX.Y.Z/`),
and `llms-full.txt` (every crate's README, `docs/DEPLOYMENT.md` and every error page in one
file) is read from the tag with `git show`. A checkout without the tag, such as CI's
shallow one, uses the working tree instead and says so; the deploy job fetches the tags.

## What updates by itself

The pages are generated when the site deploys, from the commit being deployed, so most
changes to the project reach the site without anyone touching `site/`:

- **The version** and **the list of crates** come from the workspace's `Cargo.toml`
  files (through `scripts/release.py`). A new crate appears on the home page, the crates
  page and the docs with its `Cargo.toml` description; give it an entry in
  `content.py` `CRATES` for its layer in the stack, bullets and a headline fact.
- **Lines of code** are counted from each crate's `src/`. The **test counts** are in
  `content.py` (one number per crate, summed by the build).
- **The roadmap** is `ROADMAP.md`: a `## X.Y — Name` heading is a milestone, its
  `- [x]` boxes give its progress, `(done)` or all boxes ticked marks it done.
- **The release notes** are `CHANGELOG.md`; the newest release is announced on the
  home page, with its milestone's name from the roadmap.
- **The error codes** are the tables in `docs/errors/README.md`.

The build fails on a link between the site's own pages, or to an anchor, that does not
exist. `python3 scripts/release.py check` runs it in CI on every pull request.

## Drawings

The diagrams on the home page, the docs and the benchmarks page are inline SVG written by
`art.py`: one function per idea (thread per core, one syscall per tick, provided buffers,
cancel safety, a request's path, the benchmark setup). They take their colours from the
tokens in `styles.css`, so they follow the theme, and they are drawn on a 360-unit canvas so
their text stays readable on a phone. Motion is CSS only and stops for readers who ask for
reduced motion; the still picture says everything on its own. If the design they show
changes, change the drawing in the same pull request.

## Changing the words or a number

Edit `content.py`. Every number on the site comes from the README and `bench/results/`;
when a benchmark is run again, update the README first and then `content.py`, so the
two never disagree. Keep the caveats next to the numbers they qualify.

## Looking at it locally

```bash
python3 site/build.py
python3 -m http.server -d site/public 8000              # then open http://localhost:8000
python3 site/build.py /tmp/giong-site --preview         # pages as name.html, to open as files
```

Python 3.8+ with nothing installed.

## Deploying

giong.run is served by GitHub Pages from the
[giong-run/giong-run.github.io](https://github.com/giong-run/giong-run.github.io)
repository. The [Site workflow](../.github/workflows/site.yml) builds the pages and
copies `site/public/` there (keeping that repository's `README.md` and `.nojekyll`) on
every push to `main` and at the end of every release. It holds back a version that has no
published GitHub Release yet, so the site never announces a release before crates.io has
it. Pages rebuilds within a minute.

The workflow pushes with a deploy key: the `SITE_DEPLOY_KEY` secret in this repository is
the private half of a key with write access on giong-run.github.io. Without it the workflow
only builds the site. To deploy by hand instead, run `python3 site/build.py`, copy
`site/public/` over the files in that repository and push.

The domain's DNS is on Cloudflare, set to "DNS only" so GitHub can issue the certificate:
four `A` and four `AAAA` records for the apex pointing at GitHub Pages, `www` as a `CNAME`
to `giong-run.github.io`, and the `_github-pages-challenge-giong-run` TXT record that
verifies the domain for the organisation. HTTPS is enforced.
