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
  `3`. The stall's environment holds only `PATH`, `HOME`, `LANG`, barn's own
  variables and the token.
- A stall connects back: `stall.start`, carrying its version, its token and its
  jobs, is the one message it sends first. Every other line it sends answers
  barn or follows one.
- The token is spent on the first `stall.start` that arrives with it, and it is
  never logged: `Token` prints `[redacted]`, so it cannot reach a log by
  accident.
- The state folder is not in the environment either. It arrives in barn's
  answer to `stall.start`, and a stall whose start barn has not answered has no
  state folder to write to.
- One JSON-RPC 2.0 message per line, each terminated by `\n` and never by
  `\r\n`.
- A line is at most 8 MiB in bytes, its terminating newline included,
  `MAX_LINE_BYTES` in `barn-stall`. A longer line is answered with `-32600` and
  an `id` of `null`, because the line was never parsed, and then the connection
  closes, as Chauffeur v2's socket server does. _For B1 to confirm: the error's
  message, which Chauffeur v2 words "the request line is over 1 MiB"._
- Calls in flight are bounded. _For B1 to confirm: the bound, and what a stall is
  sent when its answers would exceed it._
- Bodies are inline, and the cap counts decoded bytes: at most 4 MiB, the
  grant's `DEFAULT_MAX_BODY_BYTES` by default. A side that sends a body uses
  UTF-8 when the bytes are valid UTF-8 and the line still fits `MAX_LINE_BYTES`,
  and base64 otherwise, with `body_encoding` saying which. A body over the cap
  is refused: barn answers the stall with an error naming the cap, and a stall
  that sends one has broken the protocol. _For B1 to confirm: that error's
  code._

## Versions

`protocol` is an integer, `1` today (`PROTOCOL_VERSION` in `barn-stall`). barn
speaks the current version and the one before it. Fields are only added within a
version: a rename or a removal is a new version.

`stall.start` carries `protocol`, and it is the only message that does, in its
one direction: a stall states the version it speaks once, as it starts. barn
answers `{state}` when it speaks that version. When it does not, barn answers
`-32010`, `VERSION_REFUSED`, and marks the stall failed: it is not restarted,
because a restart would be refused the same way.

The values `body_encoding` may carry are closed within a version: another
encoding is a new version, never a value added inside this one. `auth.as` is
closed the same way, with `bearer` the only scheme this version carries.

## Lifecycle

A stall is ready when barn has answered its `stall.start` with `{state}`, and
not before.

From that answer, barn checks `stall.health` every 10 s, with a 2 s deadline on
each check. The first check comes one interval after the answer: barn does not
poll once more as soon as the stall is up, because the answer already said it
is.

A stall that does not start within the start deadline is a failed start. It
comes under the restart policy, as any other failure does.

A stall's state folder exists only once barn has answered its `stall.start`: work
that happens before that answer uses no state folder at all. A stall needs no log
file of its own either, because barn captures its stdout and stderr into the
stall's log.

## Methods

| Method                        | Direction     | Purpose                                                                                                   |
| ----------------------------- | ------------- | --------------------------------------------------------------------------------------------------------- |
| `stall.start`                 | stall to barn | Start: the version the stall speaks, its token, its own version and its jobs, which barn holds timers for |
| `stall.health`, `stall.drain` | barn to stall | Report health; finish in-flight work and exit within a deadline                                           |
| `request`, `cancel`           | barn to stall | A client request from a listener; a cancel or a closed connection                                         |
| `stream.send`, `stream.end`   | stall to barn | Write to a response stream, such as SSE                                                                   |
| `webhook`                     | barn to stall | A verified webhook delivery                                                                               |
| `job.run`                     | barn to stall | Run a declared job; barn keeps the timers and shows them in `barn stalls`                                 |
| `net.request`                 | stall to barn | One HTTP request through the broker. Errors: `denied`, `budget` (with when to retry), `unknown_outcome`   |
| `mcp.call`                    | stall to barn | Call a tool on an MCP server barn runs                                                                    |
| `log`                         | stall to barn | A structured log line                                                                                     |

`barn-stall`'s `method` module names every one of them. `stall.start` is the one
message a stall sends first, and the only one barn never sends: a stall that
receives one answers `-32601`.

## The messages fixed so far

A stall starts itself:

```json
{"jsonrpc":"2.0","id":1,"method":"stall.start","params":{"protocol":1,"stall":"sourcefed","token":"9f2c5a1e7b3d4c6f8a0b1c2d3e4f5a6b","version":"0.6.0","jobs":{"poll":{"every":"15s"},"sweep":{"every":"10m"}}}}
{"jsonrpc":"2.0","id":1,"result":{"state":"/Users/steven/.local/state/barn/stalls/sourcefed"}}
```

barn's answer is the state folder, absolute, as the stall's file in
`~/.config/barn/stalls` gives it. A job's `every` is a whole number and one
unit, `ms`, `s`, `m` or `h`: `15s` and `250ms`. `parse_duration` in
`barn-stall` reads it and refuses anything else, a zero among them.

A `stall.start` carrying a version barn does not speak is answered with
`-32010` and no state folder, and the stall is marked failed:

<!-- prettier-ignore -->
```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32010,"message":"the version is not one barn speaks","data":{"protocol":2,"speaks":[1,0]}}}
```

A `stall.start` that reaches a stall is answered `-32601`, since a stall starts
and is never started:

<!-- prettier-ignore -->
```json
{"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"a stall starts, and is never started","data":{"method":"stall.start"}}}
```

barn checks the stall's health:

```json
{"jsonrpc":"2.0","id":2,"method":"stall.health"}
{"jsonrpc":"2.0","id":2,"result":{"ok":true,"status":"ready"}}
```

barn drains it, at a restart:

```json
{"jsonrpc":"2.0","id":3,"method":"stall.drain","params":{"deadline_ms":5000}}
{"jsonrpc":"2.0","id":3,"result":{"drained":true}}
```

barn runs a job the stall declared:

```json
{"jsonrpc":"2.0","id":4,"method":"job.run","params":{"job":"poll"}}
{"jsonrpc":"2.0","id":4,"result":{"ran":true}}
```

The stall writes a log line, which barn appends to the stall's log. A log line is
a notification: it carries no `id` and barn answers nothing.

<!-- prettier-ignore -->
```json
{"jsonrpc":"2.0","method":"log","params":{"level":"info","message":"polled 12 releases"}}
```

A stall asks the broker:

```json
{"jsonrpc":"2.0","id":7,"method":"net.request","params":{"plugin":"github-releases","method":"GET","url":"https://api.github.com/repos/wrkhse/rungs/releases","auth":{"secret":"github-token","as":"bearer"},"budget":"github"}}
{"jsonrpc":"2.0","id":7,"result":{"status":200,"headers":[["etag","v1"]],"body":"hello","body_encoding":"utf8"}}
{"jsonrpc":"2.0","id":8,"error":{"code":-32001,"message":"denied","data":{"reason":"host not granted","host":"evil.example"}}}
```

A POST with headers and a UTF-8 body, and its answer, which keeps a header that
repeats:

```json
{"jsonrpc":"2.0","id":11,"method":"net.request","params":{"plugin":"github","method":"POST","url":"https://api.github.com/graphql","headers":[["content-type","application/json"],["accept","application/json"]],"body":"{\"query\":\"{ viewer { login } }\"}","body_encoding":"utf8","auth":{"secret":"github-token","as":"bearer"},"budget":"github"}}
{"jsonrpc":"2.0","id":11,"result":{"status":200,"headers":[["set-cookie","a=1"],["set-cookie","b=2"],["etag","v1"]],"body":"hello","body_encoding":"utf8"}}
```

A body that is not UTF-8, carried as base64:

```json
{"jsonrpc":"2.0","id":12,"method":"net.request","params":{"method":"PUT","url":"https://uploads.example/","body":"aGk=","body_encoding":"base64"}}
{"jsonrpc":"2.0","id":12,"result":{"status":201,"headers":[],"body":"","body_encoding":"utf8"}}
```

These bodies are `barn-stall`'s types, and each one is checked against the JSON
on this page in that crate's tests, which read this file with `include_str!`:

| Body                     | Type               |
| ------------------------ | ------------------ |
| `stall.start` parameters | `StallStartParams` |
| the start token          | `Token`            |
| `stall.start` answer     | `StallStartAnswer` |
| a declared job           | `Job`              |
| `stall.health` result    | `HealthResult`     |
| `stall.drain` parameters | `DrainParams`      |
| `stall.drain` result     | `DrainResult`      |
| `job.run` parameters     | `JobRunParams`     |
| `job.run` result         | `JobRunResult`     |
| `log` parameters         | `LogParams`        |
| a log line's level       | `LogLevel`         |
| `net.request` parameters | `NetRequestParams` |
| the secret to add        | `NetAuth`          |
| how a secret is added    | `AuthScheme`       |
| `net.request` result     | `NetResponse`      |
| the `denied` error code  | `DENIED_CODE`      |
| the `-32010` error code  | `VERSION_REFUSED`  |
| the `-32601` error code  | `NO_SUCH_METHOD`   |

The plan's sketch shows a response's headers as one JSON object and its body as
one string. Some things in HTTP cannot be carried that way, so these types go
past the sketch and B1 confirms them:

- a response's headers are a list of name and value pairs, names lower-cased,
  so a name that repeats, such as `set-cookie`, keeps every value;
- a response's body is UTF-8 text, or base64 when `body_encoding` says
  `base64`;
- a request carries its headers the same way, lower-cased, and its body inline
  with a `body_encoding` of its own;
- either side may send a header name in any case; barn lower-cases names on the
  way in and on the way out, so a stall always sees lower-case names;
- a header whose value is not valid UTF-8 is dropped, and barn logs it once per
  request with its name; the request or response still goes through.

A JSON-RPC `id` is a positive integer or absent. A string `id`, a negative one,
`0`, an `id` of `null` beside a method, a message that carries a method with a
result or an error, and params that are not an object are each refused with
`-32600`, an `id` of `null`, and the connection closes.

## Limits and constants

| Constant                 | Value     | Meaning                                                |
| ------------------------ | --------- | ------------------------------------------------------ |
| `PROTOCOL_VERSION`       | `1`       | the version barn speaks                                |
| `MAX_LINE_BYTES`         | `8388608` | 8 MiB, the longest line, its newline included          |
| `DEFAULT_MAX_BODY_BYTES` | `4194304` | 4 MiB, the default cap on a body a grant sets          |
| `DENIED_CODE`            | `-32001`  | the error code for `denied`                            |
| `VERSION_REFUSED`        | `-32010`  | the error code for a version barn cannot speak         |
| `NO_SUCH_METHOD`         | `-32601`  | the error code for a method the receiver does not take |

## For B1 to confirm

The plan fixes these methods' names and directions but not their bodies. Nothing
is built for them until B1 settles them:

- `request`: the method, params and connection id barn passes, and how a response
  stream is opened.
- `cancel`: what it names, and what a closed connection does to everything on it.
- `stream.send` and `stream.end`: their parameters, and the keepalive comments
  barn writes on SSE.
- `webhook`: the verified body and headers, and the named signature schemes,
  GitHub's HMAC-SHA256 and Slack's v0 with a 300 s window.
- `mcp.call`: its parameters and result.
- the error codes for `budget` and `unknown_outcome`, with what data they carry.
- the start deadline, and how long barn waits for a `stall.start` before the
  start has failed.
- the code of the error barn answers with when a body is over its cap.
