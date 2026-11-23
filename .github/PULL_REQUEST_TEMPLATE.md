## What and why

<!-- What does this change, and what problem does it solve? Link the issue: "Fixes #123". -->

## How it was tested

<!-- Tests added or changed, and anything you ran by hand. Kernel version if relevant. -->

## Checklist

- [ ] `scripts/preflight.sh` passes (the gate, not `--quick`) before this is marked ready for review, and so does `scripts/check.sh test` when Rust changed
- [ ] Every review thread is answered: fixed, then a reply `Fixed in <commit>` (which resolves it), or a reply saying why not
- [ ] New behaviour has tests, including what happens when an operation is dropped mid-flight
- [ ] Every new `unsafe` block has a `// SAFETY:` comment, and `scripts/valgrind.sh` and `scripts/check.sh tsan` are clean
- [ ] Performance claims come with before/after numbers and the method
- [ ] User-visible changes have an entry in a new file in `changelog.d/` (see its README)
- [ ] The pull request title follows the commit convention in `CONTRIBUTING.md`
