# AGENTS.md

barn is the only daemon on a wrkhse machine. Projects do not build daemons: each
ships a **stall**, an adapter holding only its own logic — its request handlers,
its background jobs such as polling, and the events it emits. barn starts each
stall in its own sandboxed child process, holds every listener and speaks its
protocol, and brokers the network, secrets, webhooks, MCP and upgrades.

The plan is [wrkhse/barn#1](https://github.com/wrkhse/barn/issues/1). Slice B0,
this repository and its gates, is what exists today; B1 builds the supervisor.

## Vocabulary

Say **stall**. Never "lane", and never "plugin" for a stall: a plugin is a
stall's own JavaScript, which `barn-plugin-host` runs when that crate exists.

- `barn stalls` lists the stalls, and `barn stalls restart sourcefed` drains and
  restarts one. A stall's file lives at `~/.config/barn/stalls/<name>.toml`.
- A stall ships logic only.
- barn owns everything a stall must not do itself.

| barn owns                                 | a stall ships                            |
| ----------------------------------------- | ---------------------------------------- |
| starting, restarting and upgrading stalls | request handlers                         |
| every listener, its framing and SSE       | background jobs and their schedules      |
| the sandbox each stall runs under         | the events it emits                      |
| secrets, the network and budgets          | its own records and state                |
| webhooks, MCP servers and the gateway     | nothing that touches a socket barn holds |

## Layout

| Path                     | What it holds                                                                      |
| ------------------------ | ---------------------------------------------------------------------------------- |
| `crates/barn`            | the `barn` binary: today the command line, later the supervisor                    |
| `crates/barn-stall`      | the Rust stall SDK: the protocol's version, limits, method names and message types |
| `docs/stall-protocol.md` | the protocol as it is fixed; anything not fixed is marked "for B1 to confirm"      |
| `docs/grant.md`          | the grant contract and its planned crate                                           |
| `docs/steering.md`       | the steering contract and its planned crate                                        |
| `docs/approvals.md`      | the approvals contract and its planned crate                                       |
| `docs/control.md`        | the control contract and its planned crate                                         |

## Build and gate

Rust 1.100, edition 2024, `unsafe_code = "forbid"`, clippy pedantic with
warnings denied. The toolchain is pinned in `rust-toolchain.toml` at 1.100.0,
the same version as `rust-version`, and the dependencies in `Cargo.lock`; every
command passes `--locked`.

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
cargo test --doc --workspace --locked
cargo deny check
npm ci && npm run check
```

`cargo nextest run` needs `cargo-nextest`; `cargo test --workspace` runs the
same tests without it. `npm run check` is prettier and markdownlint over the
markdown, from `package.json`. Mutation testing runs on a pull request's
changed lines only, and only in CI: dispatch the Check workflow on your branch
(`gh workflow run check.yml --ref <branch>`) and read its `mutants` job. Never
run it here.

| Gate    | What fails it                                                                                                | Job in `.github/workflows/check.yml` |
| ------- | ------------------------------------------------------------------------------------------------------------ | ------------------------------------ |
| fmt     | `cargo fmt --check`                                                                                          | `fmt`                                |
| clippy  | a lint warning, Cargo's own included (`CARGO_BUILD_WARNINGS=deny`)                                           | `clippy`                             |
| test    | a failing test or doc test                                                                                   | `test`                               |
| msrv    | anything the pinned toolchain accepts and 1.100.0 does not                                                   | `msrv`                               |
| deny    | a licence off the allow-list, a banned crate, an advisory, a yanked crate, an unknown registry or git source | `deny`                               |
| mutants | a viable mutant in a pull request's changed lines that no test catches                                       | `mutants`                            |
| docs    | markdown that prettier or markdownlint would change                                                          | `docs`                               |

Every gate runs in CI, one job each, and a deliberate break fails the job it
targets. Never weaken a gate to make a change pass: fix the change, or say why
the gate is wrong.

## Tests that touch processes

Tests that start, signal or kill a process, fuzzers and mutation runs execute
code that can affect the machine they run on. They run in CI only, **never on
the owner's Mac**. A test that needs one shows in the test itself that it is
skipped outside CI: `crates/barn/tests/exit.rs` starts the binary, so it
returns at once unless `CI` is set.

## Protocol changes

`crates/barn-stall` and `docs/stall-protocol.md` change in the same commit.
Fields are only added within a version; a rename or a removal is a new version,
and barn speaks the current version and the one before it. Anything the plan
does not fix stays marked "for B1 to confirm" and unbuilt.

## Branches

`main` is green. Every change after the first commit comes from a branch
through a pull request, and nothing force-pushes `main`.
