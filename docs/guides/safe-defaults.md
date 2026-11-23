# Safe defaults of the dev kit

The `giong` command runs code, holds secrets and opens ports on the machine you work on. It is built so that the default is the safe choice, and every way out of it is a flag you type. This page lists each default, what it protects and how to change it.

## Nothing listens beyond this machine

- A new project's `.env.example` sets `LISTEN=127.0.0.1:<port>`; only its Dockerfile sets `0.0.0.0`, for production.
- `giong dev` serves an address of every interface (`0.0.0.0`, `[::]`) on loopback instead, with the warning `GIONG-E5024`. `--lan` keeps the address, to reach the service from a phone or another computer.
- The dashboard listens on `127.0.0.1` only and answers only a loopback `Host`.

## The dashboard needs its token

Each `giong dashboard` or `giong dev --ui` makes a random token, printed as part of its address (`http://127.0.0.1:7700/#t=...`). Every API call needs it in the `X-Giong-Dashboard` header, or gets `403` with `GIONG-E5051`. The token comes after `#`, so no request line, log or `Referer` carries it, and the page takes it out of the address bar once read. A web page, another program or another user on the machine cannot read the dashboard or send requests through it.

## Recorded requests keep secrets out

`giong record save` writes real requests as test cases that are meant to be committed, so:

- Redacted headers, and fields whose names say secret (`password`, `token`, `api_key`, `client_secret`, `card_number`, `cvv` and the like) in a query, a form or a JSON body, become `{{env:GIONG_RECORD_<NAME>}}` and are read from the environment when the cases run. The same fields in answers become `{{string}}`.
- A value that still looks like a secret, such as a card number (by the Luhn check), a JWT or a value under a secret name, stops the save with `GIONG-E5050`, and nothing is written. `--keep-secrets` saves anyway, with warnings, for test values such as the `4242 4242 4242 4242` card.
- giong-web's `Record` middleware redacts the same names as `[redacted]` (`redact_field` and `keep_field` change the list), and it is compiled out of release builds unless the `dev` feature is on (`GIONG-E1213`).

## Agents read; they change things only when you allow it

`giong mcp` serves the commands that only read. Commands that change the project need `giong mcp --allow-writes`. Emptying or removing a database, `giong clean --all`, printing a secret value (`giong env get`, `--reveal`) and pushing an image are never tools.

## Development code stays out of production

`giong dev` hands its listening sockets to the service it starts, so a new build can take over without refusing a connection. The runtime takes a socket only from the `giong dev` that started it, and only in debug builds or with giong's `dev` feature, which `giong dev --release` and `--prod-like` turn on for their own builds. A release build without it refuses to start under `giong dev` instead of taking the socket.

## Secrets stay in .env, and .env stays on this machine

- `giong db up` starts each database with a random password and writes its URL to `.env`, which it makes readable by you only and lists in `.gitignore`.
- Each checkout gets its own container, so two copies of a project never share a database; a container giong did not record is refused with `GIONG-E5049` instead of guessing its password.
- `.env` is read in development only (`giong dev`, `giong test`); in production the service takes its settings from the environment. `giong env show` masks secret values.

## The install script checks what it downloads

`curl -fsSL https://giong.run/install.sh | sh` downloads the release binary from GitHub, checks its SHA-256 and puts it in `~/.giong/bin` without asking for root. `GIONG_VERSION` pins a version.
