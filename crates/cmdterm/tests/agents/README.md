# Agent harness

The tests in `crates/cmdterm/src/tests/agent_harness.rs` run the real Claude Code,
Codex and Qwen Code in a podman container. The app drives them from its own
terminals, with the hooks its settings page installs, and the agents talk to
`fake_api.py` instead of a real model. The checks cover what unit tests with a
stand-in agent cannot:

- An agent started with a prompt comes back after a restart in the same session,
  and the prompt does not run again.
- An agent stopped in the middle of a turn gets the prompt to continue.
- An agent behind `podman exec` is not frozen, since stopping the client would
  not stop the agent.

## Running

```sh
cargo test -p cmdterm --lib agent_harness -- --ignored --test-threads=1
# One agent:
cargo test -p cmdterm --lib agent_harness::codex -- --ignored --test-threads=1
```

The tests need podman. The first run builds `localhost/scriptschnell-term-agents:dev`
from `Containerfile`, which needs the network; the containers themselves run with
`--network=none`.

| Variable | Effect |
| --- | --- |
| `CMDTERM_HARNESS_REBUILD=1` | Builds the image again, to pick up new agent releases. Pin versions with build args such as `CODEX=0.166.0` in `Containerfile`. |
| `CMDTERM_HARNESS_KEEP=1` | Keeps the container after the test. Remove it with `podman rm -f cmdterm-harness-...`. |
| `CMDTERM_HARNESS_PAUSE=1` | When a wait times out, sleeps 15 minutes before failing, so the agent and the app are still running when you look. |
| `HARNESS_SLOW` | Seconds `fake_api.py` waits before answering a prompt with `SLOW` in it (default 600). |

## Sandbox tests

`crates/cmdterm/src/tests/agent_harness/sandbox.rs` runs the agents on this
machine, in the app's own sandbox (bubblewrap on Linux), next to
`fake_forge.py`. The shipped `git-credentials` and `npm` extensions give the
sandbox fakes of the forge's logins, and the fake API has the agent run
`git fetch`, `git push` and `npm publish` through its shell tool. The tests
check that:

- the sandbox holds only fakes, and the forge gets the real logins through
  the proxy;
- a guard set to allow passes pushes without a question, and one set to
  block refuses them without one;
- a guard set to ask holds a push or an upload until the test answers, and
  the forge sees it only when the answer allows it;
- no real login reaches the model.

```sh
# A plain shell in place of the agent; these run with the other tests.
cargo test -p cmdterm --lib agent_harness::sandbox
# The real agents:
cargo test -p cmdterm --lib agent_harness::sandbox -- --ignored --test-threads=1
```

They need bubblewrap, python3, git and npm. The first run of the ignored
tests installs the agents with npm into `target/agent-harness/npm`, which
needs the network; `CMDTERM_HARNESS_REBUILD=1` installs them again. The forge
listens on `127.0.0.2`, which the proxy variables' `NO_PROXY` leaves out, so
its traffic goes through the proxy. Seatbelt on macOS hides
`~/.git-credentials` instead of faking it, so the tests skip there.

## Files

- `Containerfile` installs the agents on Node 22 with python3, git and bubblewrap.
- `fake_api.py` answers the Anthropic Messages API, the OpenAI Responses API and
  Chat Completions. Each reply says how many prompts the conversation held, so a
  resumed session answers its next prompt with `turn=2`. Every request goes to
  `$HARNESS_STATE/requests.jsonl`. Only lines with `TASK` in them, and the app's
  prompt to continue, count as prompts; the agents' own calls are marked `side`.
  A prompt with `RUN: COMMAND` gets a call of the agent's shell tool, and the
  tool's output lands in the log as `tool_output`.
- `fake_forge.py` serves bare git repositories through `git http-backend`
  and takes `npm publish` uploads, only with the real logins, and logs every
  request to `$FORGE_STATE/forge.jsonl`.
- `setup.sh` skips onboarding, points each agent at the fake API
  (`HARNESS_PORT`, default 8787), trusts the test repository and lets the
  agents run shell commands without asking.
- `trust_codex_hooks.py` starts Codex once and answers its question about new
  hooks with "trust all", as a user does once after turning the integration on.
  Codex does not run hooks it has not been told to trust.

The scripts are mounted into the container at `/opt/harness`, so changing them
needs no new image.

## Codex

Codex 0.166 moves sessions into a shared background server by default. Hooks run
there inherit that server's environment and have no terminal, so they cannot tell
the app which terminal a session is in. The integration sets
`daemon_auto_start = false` in `[features]` of `~/.codex/config.toml` to keep each
session in its own process. A server that is already running keeps serving new
sessions until it exits.
