# Changelog entries

A pull request that changes something a user would notice adds its `CHANGELOG.md` entry
here, as a file of its own, instead of editing `CHANGELOG.md`. Pull requests in flight then
never edit the same lines, so one merging cannot put the next out of date or push it out of
the merge queue. The release pull request (`scripts/release.py prepare`) merges every file
into the new version's section, under the right headings, and deletes them.

One file per pull request, named after the change in lower case, such as
`fix-unix-stream-eof.md` or `windows-iocp-driver.md` (letters, digits, `.`, `-`, `_`). It
holds `### <section>` headings from [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
(`Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`), each with list items
written exactly as they will read in `CHANGELOG.md`: grouped by crate, saying what changed
for the user and why.

```markdown
### Fixed

- **giong**: `UnixStream::read` sees the end of a stream that Linux 6.18 does not report
  ([#164](https://github.com/giong-run/Giong/pull/164)).
```

`python3 scripts/release.py check` (run by CI's `lint` job) checks the files, and
`python3 scripts/release.py unreleased` prints the section the next release will get.
Entries already under `## [Unreleased]` in `CHANGELOG.md` stay where they are and come
first in each section.
