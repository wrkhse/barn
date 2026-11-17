# The stall protocol

**Status: draft, version 1.** This page records what the plan already fixes and
nothing more. Everything barn's stall code will settle in B1 is marked "for B1
to confirm", with the question named.

A stall is a project's logic in its own child process: its request handlers, the
jobs it runs and the events it emits. barn starts it, restarts it, holds its
listeners, and gives it no network and no secrets. The two speak this protocol.
The plan is [wrkhse/barn#1](https://github.com/wrkhse/barn/issues/1).

## Transport

- barn creates a Unix socket pair and passes the stall's end as file descriptor
  `3`. The stall's environment holds only `PATH`, `HOME`, `LANG` and barn's own
  variables.
- One JSON-RPC 2.0 message per line, each terminated by `\n`.
- A line is at most 1 MiB in bytes without its terminator, `MAX_LINE_BYTES` in
  `barn-stall`. A longer line is a protocol error.
- Calls in flight are bounded. _For B1 to confirm: the bound, and what a stall is
  sent when its answers would exceed it._
- Bodies are inline. A grant caps them at `DEFAULT_MAX_BODY_BYTES`, 4 MiB by
  default. _For B1 to confirm: whether streaming is ever added, which the plan
  adds only if a fixture needs it._

## Versions

`protocol` is an integer, `1` today (`PROTOCOL_VERSION` in `barn-stall`). barn
speaks the current version and the one before it. Fields are only added within a
version: a rename or a removal is a new version.

## Methods

| Method                                       | Direction     | Purpose                                                                                                 |
| -------------------------------------------- | ------------- | ------------------------------------------------------------------------------------------------------- |
| `stall.start`, `stall.health`, `stall.drain` | barn to stall | Start, which returns the stall's jobs; report health; finish in-flight work and exit within a deadline  |
| `request`, `cancel`                          | barn to stall | A client request from a listener; a cancel or a closed connection                                       |
| `stream.send`, `stream.end`                  | stall to barn | Write to a response stream, such as SSE                                                                 |
| `webhook`                                    | barn to stall | A verified webhook delivery                                                                             |
| `job.run`                                    | barn to stall | Run a declared job; barn keeps the timers and shows them in `barn stalls`                               |
| `net.request`                                | stall to barn | One HTTP request through the broker. Errors: `denied`, `budget` (with when to retry), `unknown_outcome` |
| `mcp.call`                                   | stall to barn | Call a tool on an MCP server barn runs                                                                  |
| `log`                                        | stall to barn | A structured log line                                                                                   |

`barn-stall`'s `method` module names every one of them.

## The messages fixed so far

barn starts a stall:

```json
{"jsonrpc":"2.0","id":1,"method":"stall.start","params":{"protocol":1,"stall":"sourcefed","state":"~/.local/state/sourcefed"}}
{"jsonrpc":"2.0","id":1,"result":{"ready":true,"version":"0.6.0","jobs":{"poll":{"every":"15s"}}}}
```

A stall asks the broker:

```json
{"jsonrpc":"2.0","id":7,"method":"net.request","params":{"plugin":"github-releases","method":"GET","url":"https://api.github.com/repos/wrkhse/rungs/releases","auth":{"secret":"github-token","as":"bearer"},"budget":"github"}}
{"jsonrpc":"2.0","id":7,"result":{"status":200,"headers":{"etag":"…"},"body":"…"}}
{"jsonrpc":"2.0","id":8,"error":{"code":-32001,"message":"denied","data":{"reason":"host not granted","host":"evil.example"}}}
```

These bodies are `barn-stall`'s types, and each one is checked against the plan's
own text in that crate's tests:

| Body                     | Type               |
| ------------------------ | ------------------ |
| `stall.start` parameters | `StallStartParams` |
| `stall.start` result     | `StallStartResult` |
| a declared job           | `Job`              |
| `net.request` parameters | `NetRequestParams` |
| the secret to add        | `NetAuth`          |
| `net.request` result     | `NetResponse`      |
| the `denied` error code  | `DENIED_CODE`      |

_For B1 to confirm: whether a JSON-RPC id is an integer, a string or both, and
whether `net.request`'s `auth.as` has values beyond `bearer`._

## Limits and constants

| Constant                 | Value     | Meaning                                         |
| ------------------------ | --------- | ----------------------------------------------- |
| `PROTOCOL_VERSION`       | `1`       | the version barn speaks                         |
| `MAX_LINE_BYTES`         | `1048576` | 1 MiB, the longest line, without its terminator |
| `DEFAULT_MAX_BODY_BYTES` | `4194304` | 4 MiB, the default cap on a body a grant sets   |
| `DENIED_CODE`            | `-32001`  | the error code for `denied`                     |

## For B1 to confirm

The plan fixes these methods' names and directions but not their bodies. Nothing
is built for them until B1 settles them:

- `stall.health`: its result, the short status it carries, and the deadline barn
  gives it.
- `stall.drain`: its parameters and the deadline a stall must finish in.
- `request`: the method, params and connection id barn passes, and how a response
  stream is opened.
- `cancel`: what it names, and what a closed connection does to everything on it.
- `stream.send` and `stream.end`: their parameters, and the keepalive comments
  barn writes on SSE.
- `webhook`: the verified body and headers, and the named signature schemes,
  GitHub's HMAC-SHA256 and Slack's v0 with a 300 s window.
- `job.run`: its parameters, and whether a job carries anything but `every`.
- `mcp.call`: its parameters and result.
- `log`: the fields of a structured log line.
- the error codes for `budget` and `unknown_outcome`, with what data they carry.
- whether `Job.every` is a duration grammar, and which one.
- whether `stall.start`'s `state` arrives expanded or as `~`.
