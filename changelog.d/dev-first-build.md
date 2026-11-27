### Added

- **giong-cli**: a new project's first dev build takes the dependency builds your other
  projects already made. Before a dev build, `giong dev` adds them to the project's
  `target/debug`: cargo's own unit hash names each one, and cargo checks each fingerprint and
  builds again whatever differs. A unit comes only with its dependencies from the same project,
  or when the dependencies here have the same bytes. Then cargo compiles only what is left.
  Eight projects from `giong new` built one after another: first build plus first test build
  of all eight went from 131.0 s to 58.7 s (-55%). The seven after the first went from
  109.0 s to 34.7 s (-68%). A second web + SQLite project went from 23.6 s to 3.1 s.
  Edit-and-rebuild times stay the same, and release builds are always cargo's own.
  `giong cache fill` does it by hand.
