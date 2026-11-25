# Build with a coding agent

Gióng is made for code that agents write: one obvious way to do each job, errors that say how to fix them, and a `giong` command that answers in JSON. This page is the setup and the loop to give an agent, whether you type every prompt or let it run on its own.

## Set the project up once

`giong new` writes a project that already teaches agents how to work on it. For a project you already have, run the same step on its own:

```sh
giong new api --db postgres   # a new project, with the files below
giong ai init                 # an existing project: the same files, nothing else touched
```

It writes:

| File | Read by |
|---|---|
| `AGENTS.md` | Codex, Copilot, Cursor, Gemini and most other agents: the Gióng rules and this project's commands |
| `CLAUDE.md` | Claude Code; it imports `AGENTS.md` |
| `.cursor/rules/giong.mdc` | Cursor |
| `.claude/skills/giong/` | Agents that load skills: the Gióng skill of this release |
| `.mcp.json` | Agents that speak MCP: the `giong` server (`giong mcp`) |

`giong upgrade` runs it again when it moves the project to a new release; run it yourself after moving giong by hand. In a file you wrote, it replaces only its own marked block; in `.mcp.json` it adds its server and keeps everything else.

## The loop an agent follows

One project has one runner, and the agent drives it with these commands only:

```sh
giong dev --background          # once: builds, serves on 127.0.0.1, returns when it answers
# ... edit the code: the runner rebuilds on every save and swaps the new build in ...
curl http://127.0.0.1:8080/health
giong dev status --json         # is it up, where, and why not (failure: code, cause, last lines, fix)
giong dev logs --lines 50       # what the service printed
giong check --json              # compiler, clippy and giong's own checks: run before calling it done
giong fix                       # applies the fixes that need no judgement
giong test                      # the tests, on io_uring and on epoll
giong dev stop                  # at the end: the service, and the database giong started
```

What the agent must not do while developing: `cargo run`, `cargo build --release`, start Docker or a container itself, or open a second terminal to run the service. Each of those starts a second copy that fights the runner for the port. A change that does not compile, or that compiles but does not start (a panic at start-up, a port it cannot bind), stops the service instead of leaving an older build answering: the agent never tests code that is not in the files. Requests are refused until the code works, and `giong dev status --json` says why: `state` is `failed` and `failure` holds the code (`GIONG-E5009` for the compiler, `GIONG-E5017` for a service that stopped, `GIONG-E5014` for a port in use, `GIONG-E5018` for one that never listened), the cause, the compiler's errors or the service's last lines in `detail`, and the fix. The agent fixes the code and saves; the runner builds and serves again by itself.

## Connect the agent to giong over MCP

`.mcp.json` already names the server for agents that read it. In Claude Code, by hand:

```sh
claude mcp add giong -- giong mcp
```

Every command that only reads is a tool: `check`, `doctor`, `explain`, `routes`, `top`, `dev` for `status` and `logs`. Commands that change the project (`fix`, `dev --background`, `dev stop`, ...) are tools only when you start the server with `giong mcp --allow-writes`. Some are never tools, whatever the flag: emptying or removing a database, `clean --all`, reading a secret value, pushing an image.

While `giong dev` runs the service, the agent can also ask the service itself, through [giong-mcp](../../giong-mcp/README.md), which task blocks a worker, which line spawned the tasks that keep growing, and how full the pools are. That is faster and more certain than guessing from the code.

## Read what giong reports

Every command takes `--json` and answers with one document. Each problem is a finding with a code, the place in the source, the cause and the fix:

```json
{"command": "check", "ok": false,
 "findings": [{"code": "GIONG-E5006", "severity": "warning",
               "title": "std::fs blocks the worker inside an async function",
               "fix": "...", "file": "src/main.rs", "line": 14, "column": 19}]}
```

The exit status is 0 when nothing is wrong, 1 when a finding is an error, and 2 when the command could not run. Every code has a page: `giong explain GIONG-E5006` prints it without the network, and the [error index](../errors/README.md) lists them all.

## Give the agent the documentation

- `https://giong.run/llms.txt`: what an agent needs first, with links to everything else.
- `https://giong.run/llms-full.txt`: every README, the guides and every error page in one file.
- Every page of giong.run is also Markdown: add `.md` to its address, or press **Copy for agent**.
- **Copy prompt** on any page copies the rules and that page, ready to paste with your task.

## A first prompt

```text
This is a Rust service on Gióng. Read AGENTS.md first.
Start it with `giong dev --background` if `giong dev status` says it is not running,
then: <what to build or fix>.
Check your work with curl against 127.0.0.1, then `giong check --json` and `giong test`.
Do not use cargo run, a release build or Docker. Stop with `giong dev stop` when done.
```
