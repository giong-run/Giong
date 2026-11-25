### Security

- **giong-cli**: `giong doctor` reads `.env` and `.env.example` only as plain files, as
  `giong env`, `giong db` and `giong bench` already do: a cloned repository that commits
  `.env.example -> ~/.aws/credentials` no longer gets the key names of that file into the
  report. A linked file is reported, not read.
