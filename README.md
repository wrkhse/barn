# barn

barn is the only daemon on a wrkhse machine. Every project ships a **stall** —
its own logic, in a child process — and barn owns the rest: starting, restarting
and upgrading it, every listener clients connect to, logs and health, the
sandbox, secrets, the network, webhooks and MCP.

## Status

The repository and its gates are in place (slice B0). The supervisor is B1, so
the commands below are the surface the plan fixes: each one says what is not
built yet and exits non-zero.

```sh
barn status
barn stalls
barn stalls restart sourcefed
barn run sourcefed
barn logs sourcefed --follow
```

The plan is [wrkhse/barn#1](https://github.com/wrkhse/barn/issues/1). The
protocol's fixed parts are in [docs/stall-protocol.md](docs/stall-protocol.md),
and how to build and gate is [AGENTS.md](AGENTS.md).

## Layout

| Path | What it holds |
| --- | --- |
| `crates/barn` | the `barn` binary and the library it stands on |
| `crates/barn-stall` | the Rust stall SDK |
| `docs` | the stall protocol and what the plan fixes |
