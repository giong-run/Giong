### Security

- **giong-cli**: `giong openapi --out` refuses a symbolic link at the file or at a folder
  between it and the project, as `giong record save` does, so a repository that commits
  `docs -> ~/.config` cannot have the document written outside the project
  ([#397](https://github.com/giong-run/Giong/issues/397)).
