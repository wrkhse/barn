# barn

barn is the only daemon on a wrkhse machine. Each project ships a **stall**
holding only its own logic: its request handlers, its background jobs and the
events it emits.

barn starts each stall in its own sandboxed child process, holds every
listener, and brokers the network, secrets, webhooks, MCP and upgrades.

## Install

With Rust 1.100.0 installed, install the CLI from the pinned source revision:

```sh
cargo install --git https://github.com/wrkhse/barn \
  --rev 521d5f88e565244e72cb2071815579afbb23a038 --locked barn
```

## Check the version

```text
$ barn --version
barn 0.1.0
```

## Write a stall in Rust

```toml
[dependencies]
barn-stall = { git = "https://github.com/wrkhse/barn", rev = "521d5f88e565244e72cb2071815579afbb23a038" }
```

The crate holds the [stall protocol](docs/stall-protocol.md)'s version, limits,
method names and message types.

## Roadmap

- [x] Rust stall SDK: the protocol's version, limits, method names and message
      types
- [x] The `barn` command line: `status`, `stalls`, `run` and `logs`
- [ ] The supervisor behind those commands: start, restart and upgrade stalls,
      hold their listeners ([barn#1](https://github.com/wrkhse/barn/issues/1))
- [ ] [Broker](https://github.com/wrkhse/barn/issues/9),
      [gateway](https://github.com/wrkhse/barn/issues/10),
      [sandbox](https://github.com/wrkhse/barn/issues/11) and
      [conformance](https://github.com/wrkhse/barn/issues/12)
- [ ] [The workhorse stall and CLI](https://github.com/wrkhse/barn/issues/13)
- [ ] Contracts: [grant](https://github.com/wrkhse/barn/issues/14),
      [steering](https://github.com/wrkhse/barn/issues/15),
      [approvals](https://github.com/wrkhse/barn/issues/16),
      [control](https://github.com/wrkhse/barn/issues/17)

## More

- [Stall protocol](docs/stall-protocol.md)
- [Contributor and agent guidance](AGENTS.md)
