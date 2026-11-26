# The stall protocol

**Status: version 1 is a draft until barn's first release.** The page records
what the plan fixes and names the open questions under "for B1 to confirm".

A stall is a project's logic in its own child process: its request handlers, the
jobs it runs and the events it emits. barn starts it, restarts it, holds its
listeners, and gives it no network and no secrets. The two speak this protocol.
The plan is [wrkhse/barn#1](https://github.com/wrkhse/barn/issues/1).

## Transport

- barn listens at `state_dir/run/<stall>/proto.sock`, a Unix socket with mode
  `0600` in a directory with mode `0700`. The stall's environment holds only
  `PATH`, `HOME`, `LANG`, the socket path in `BARN_SOCKET`, and the single-use
  token in `BARN_TOKEN`. barn clears a stale run directory before binding the
  socket, leaving the stall's state folder intact.
- The stall connects and sends `stall.start` as its first message. barn checks
  that the peer PID matches the child it spawned and that the peer UID owns
  the socket. A missing peer PID fails authentication. The stall declares its
  jobs by name in the message's `jobs` map; barn keeps them under those names
  and returns the map in the result.
  barn spends the token, then returns the absolute path of the stall's state
  folder, `state_dir/<stall>`, which it creates and grants the stall read-write
  access to.
- The token expires `TOKEN_TTL_SECS` (30 seconds) after barn spawns the stall.
  A `stall.start` at or after that deadline is refused as `TokenExpired`: barn
  drops the token unspent and closes the connection without a reply, so only a
  restart issues a new one.
- One JSON-RPC 2.0 message per line, each terminated by `\n`.
- A line is at most 8 MiB in bytes, its terminating newline included,
  `MAX_LINE_BYTES` in `barn-stall`. A longer request line is answered with
  `-32600` and an `id` of `null`, because the line was never parsed, and then the
  connection closes, as Chauffeur v2's socket server does. barn's `stall.start` reply
  is held to the same limit: a reply that would be longer is never sent, and
  the stall gets the same `-32600` line before the connection closes. The
  error line is fixed under "The messages fixed so far".
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

`protocol` is an integer, `1` (`PROTOCOL_VERSION` in `barn-stall`). barn speaks
the current version and the one before it. Fields are only added within a
version: a rename or a removal is a new version.

The values `body_encoding` may carry are closed within a version: another
encoding is a new version, never a value added inside this one.

## Methods

| Method                        | Direction     | Purpose                                                                                                 |
| ----------------------------- | ------------- | ------------------------------------------------------------------------------------------------------- |
| `stall.start`                 | stall to barn | Authenticate the stall and return its state folder                                                      |
| `stall.health`, `stall.drain` | barn to stall | Report health; finish in-flight work and exit within a deadline                                         |
| `request`, `cancel`           | barn to stall | A client request from a listener; a cancel or a closed connection                                       |
| `stream.send`, `stream.end`   | stall to barn | Write to a response stream, such as SSE                                                                 |
| `webhook`                     | barn to stall | A verified webhook delivery                                                                             |
| `job.run`                     | barn to stall | Run a declared job; barn keeps the timers and shows them in `barn stalls`                               |
| `net.request`                 | stall to barn | One HTTP request through the broker. Errors: `denied`, `budget` (with when to retry), `unknown_outcome` |
| `mcp.call`                    | stall to barn | Call a tool on an MCP server barn runs                                                                  |
| `log`                         | stall to barn | A structured log line                                                                                   |

`barn-stall`'s `method` module names every one of them.

## The messages fixed so far

A stall connects and receives its state folder:

```json
{"jsonrpc":"2.0","id":1,"method":"stall.start","params":{"protocol":1,"token":"…","jobs":{"poll":{"every":"15s"}}}}
{"jsonrpc":"2.0","id":1,"result":{"state":"/var/lib/barn/sourcefed","jobs":{"poll":{"every":"15s"}}}}
```

A reply too long to fit is replaced by one error line:

<!-- prettier-ignore -->
```json
{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"stall start message exceeds the protocol line limit"}}
```

barn sends `stall.health` on the authenticated connection every 10 seconds:

```json
{"jsonrpc":"2.0","id":2,"method":"stall.health"}
{"jsonrpc":"2.0","id":2,"result":null}
```

A successful JSON-RPC reply with the matching id within 2 seconds answers the
check. barn buffers non-health frames on the connection during the check.
A check without a matching success is missed. barn restarts the stall after
three consecutive misses.

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

| Body                     | Type                    |
| ------------------------ | ----------------------- |
| `stall.start` parameters | `StallStartParams`      |
| `stall.start` result     | `StallStartResult`      |
| the jobs by name         | `BTreeMap<String, Job>` |
| `net.request` parameters | `NetRequestParams`      |
| the secret to add        | `NetAuth`               |
| `net.request` result     | `NetResponse`           |
| the `denied` error code  | `DENIED_CODE`           |

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

_For B1 to confirm: whether a JSON-RPC id is an integer, a string or both, and
whether `net.request`'s `auth.as` has values beyond `bearer`._

## Limits and constants

| Constant                 | Value     | Meaning                                  |
| ------------------------ | --------- | ---------------------------------------- |
| `PROTOCOL_VERSION`       | `1`       | the version barn speaks                  |
| `MAX_LINE_BYTES`         | `8388608` | 8 MiB per line, including the newline    |
| `DEFAULT_MAX_BODY_BYTES` | `4194304` | 4 MiB, the default body cap a grant sets |
| `DENIED_CODE`            | `-32001`  | the error code for `denied`              |
| `TOKEN_TTL_SECS`         | `30`      | seconds before a start token expires     |

## For B1 to confirm

The plan fixes these methods' names and directions. These details remain open:

- `stall.health`: the short status its result carries.
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
- how barn reports an unsupported version: the stall sends its version in
  `stall.start`'s `protocol` field.
- the code of the error barn answers with when a body is over its cap.
