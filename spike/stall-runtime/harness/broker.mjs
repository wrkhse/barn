// The barn stand-in. It owns the stall's fd 3, routes JSON-RPC both ways, forwards the
// stall's outbound HTTP to the mock upstream, and timestamps every request with the
// harness's own monotonic clock. A stall protocol line is one JSON object per \n and must
// fit under 1 MiB; a larger upstream body would need base64 chunking, which is out of scope
// here, so it becomes a -32001 error reply rather than a truncated frame.
import http from "node:http"

export const MAX_LINE_BYTES = 1024 * 1024
const UPSTREAM_TIMEOUT_MS = 10_000

export function startBroker({ child, upstreamBase, faults = [] }) {
  const protocol = child.stdio[3]
  if (!protocol) throw new Error("stall-spike: child.stdio[3] is missing; spawn the stall with stdio ['ignore','pipe','pipe','pipe']")

  const upstream = new URL(upstreamBase)
  const pending = new Map()
  const requestLog = []
  const counters = { requests: 0, denials: 0, malformedLines: 0, lineBytesTotal: 0 }
  let nextId = 1
  let nextSeq = 0
  let netListener = null
  let closed = false
  let buffer = Buffer.alloc(0)
  let discarding = false

  const onExit = (code, signal) => failAll(new Error(`stall exited (code ${code}, signal ${signal}) before replying`))
  child.once("exit", onExit)
  protocol.on("data", onData)
  protocol.on("error", (error) => failAll(new Error(`stall protocol socket failed: ${error.message}`)))

  function failAll(error) {
    closed = true
    for (const entry of pending.values()) {
      clearTimeout(entry.timer)
      entry.reject(error)
    }
    pending.clear()
  }

  function onData(chunk) {
    counters.lineBytesTotal += chunk.length
    const data = buffer.length > 0 ? Buffer.concat([buffer, chunk]) : chunk
    buffer = Buffer.alloc(0)
    let start = 0
    while (true) {
      const newline = data.indexOf(10, start)
      if (newline === -1) {
        const rest = data.subarray(start)
        if (discarding) return
        if (rest.length > MAX_LINE_BYTES) {
          counters.malformedLines += 1
          discarding = true
          return
        }
        if (rest.length > 0) buffer = Buffer.from(rest)
        return
      }
      const line = data.subarray(start, newline)
      start = newline + 1
      if (discarding) {
        discarding = false
        continue
      }
      if (line.length > MAX_LINE_BYTES) {
        counters.malformedLines += 1
        continue
      }
      if (line.length > 0) handleLine(line)
    }
  }

  function handleLine(line) {
    let message
    try {
      message = JSON.parse(line.toString("utf8"))
    } catch {
      counters.malformedLines += 1
      return
    }
    if (!message || typeof message !== "object" || Array.isArray(message)) {
      counters.malformedLines += 1
      return
    }
    if (typeof message.method === "string") {
      if (message.id === undefined || message.id === null) {
        if (message.method !== "log") counters.malformedLines += 1
        return
      }
      handleRequest(message)
      return
    }
    if (message.id === undefined) {
      counters.malformedLines += 1
      return
    }
    settle(message)
  }

  function settle(message) {
    const entry = pending.get(message.id)
    if (!entry) return
    pending.delete(message.id)
    clearTimeout(entry.timer)
    if (message.error) {
      const error = new Error(`${entry.method} failed: ${message.error.message ?? "unknown error"}`)
      error.code = message.error.code
      error.data = message.error.data
      entry.reject(error)
    } else {
      entry.resolve(message.result)
    }
  }

  function handleRequest(message) {
    if (message.method === "net.request") {
      forward(message).catch((error) => {
        counters.denials += 1
        sendError(message.id, -32001, `net.request failed: ${error.message}`)
      })
      return
    }
    if (message.method === "log") {
      sendLine({ jsonrpc: "2.0", id: message.id, result: { ok: true } })
      return
    }
    counters.denials += 1
    sendError(message.id, -32601, `unknown stall method ${message.method}`, { method: message.method })
  }

  async function forward(message) {
    const params = message.params ?? {}
    if (typeof params.url !== "string" || typeof params.method !== "string") {
      counters.denials += 1
      sendError(message.id, -32602, "net.request needs method and url strings", { params })
      return
    }

    counters.requests += 1
    const tSentNs = process.hrtime.bigint()
    const entry = { seq: (nextSeq += 1), monitorKey: keyForUrl(params.url), url: params.url, tSentNs, tDoneNs: null, status: null, bytes: 0 }
    requestLog.push(entry)
    if (netListener) {
      try {
        netListener({ tNs: tSentNs, plugin: params.plugin ?? null, url: params.url, monitorKey: entry.monitorKey })
      } catch (error) {
        process.stderr.write(`stall-spike: onNetRequest listener failed: ${error.message}\n`)
      }
    }

    const target = new URL(upstreamBase)
    const source = new URL(params.url)
    target.pathname = source.pathname
    target.search = source.search

    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), UPSTREAM_TIMEOUT_MS)
    timer.unref()
    try {
      const init = { method: params.method, headers: params.headers ?? {}, signal: controller.signal }
      if (params.body !== undefined && params.method !== "GET" && params.method !== "HEAD") init.body = params.body
      const response = await fetch(target, init)
      const body = await response.text()
      const headers = {}
      for (const [name, value] of response.headers) headers[name.toLowerCase()] = value
      const bodyBytes = Buffer.byteLength(body)

      if (bodyBytes + 512 > MAX_LINE_BYTES) {
        counters.denials += 1
        sendError(message.id, -32001, `upstream body of ${bodyBytes} bytes exceeds the 1 MiB stall protocol cap; base64 chunking is out of scope for the spike`, { bytes: bodyBytes })
      } else {
        sendLine({ jsonrpc: "2.0", id: message.id, result: { status: response.status, headers, body } })
        entry.status = response.status
        entry.bytes = bodyBytes
      }
      // tDone is when the reply was handed to the stall protocol, the last instant the
      // harness can observe; the stall's remaining work and its own delivery follow this stamp.
      entry.tDoneNs = process.hrtime.bigint()
    } catch (error) {
      counters.denials += 1
      sendError(message.id, -32001, `upstream ${params.url} failed: ${error.message}`, { url: params.url, cause: error.message })
      entry.tDoneNs = process.hrtime.bigint()
    } finally {
      clearTimeout(timer)
    }
  }

  function sendLine(message) {
    const line = `${JSON.stringify(message)}\n`
    const bytes = Buffer.byteLength(line)
    if (bytes > MAX_LINE_BYTES) throw new Error(`stall protocol line of ${bytes} bytes exceeds the 1 MiB cap`)
    if (closed) throw new Error("the stall protocol is closed")
    protocol.write(line)
  }

  function sendError(id, code, message, data = {}) {
    try {
      sendLine({ jsonrpc: "2.0", id, error: { code, message, data } })
    } catch (error) {
      process.stderr.write(`stall-spike: could not send the stall protocol error: ${error.message}\n`)
    }
  }

  function request(method, params, timeoutMs) {
    if (closed) return Promise.reject(new Error(`cannot send ${method}: the stall protocol is closed`))
    const id = nextId
    nextId += 1
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id)
        reject(new Error(`${method} timed out after ${timeoutMs} ms`))
      }, timeoutMs)
      timer.unref()
      pending.set(id, { resolve, reject, timer, method })
      try {
        sendLine({ jsonrpc: "2.0", id, method, params })
      } catch (error) {
        clearTimeout(timer)
        pending.delete(id)
        reject(error)
      }
    })
  }

  async function startStall(options = {}, timeoutMs = 15_000) {
    const params = { protocol: 1, stall: "sourcefed", http: { host: "127.0.0.1", port: 0 }, faults, ...options }
    const result = await request("stall.start", params, timeoutMs)
    if (!result || result.ready !== true) throw new Error(`stall.start did not report ready: ${JSON.stringify(result)}`)
    return result
  }

  const health = (timeoutMs = 2000) => request("stall.health", {}, timeoutMs)
  const drain = (timeoutMs = 5000) => request("stall.drain", {}, timeoutMs)

  function stop() {
    protocol.removeListener("data", onData)
    child.removeListener("exit", onExit)
    failAll(new Error("broker stopped"))
    try {
      protocol.destroy()
    } catch {
      // The socket may already be closed with the child; nothing left to release.
    }
  }

  return {
    startStall,
    health,
    drain,
    stats: () => ({ ...counters }),
    requestLog: () => requestLog.slice(),
    onNetRequest(listener) {
      netListener = listener
      return () => {
        if (netListener === listener) netListener = null
      }
    },
    stop,
  }
}

/**
 * The monitor identity a stall protocol request belongs to, from its URL. It is the key
 * that pairs a delivered event with the request that produced it.
 */
export function keyForUrl(rawUrl) {
  let url
  try {
    url = new URL(rawUrl)
  } catch {
    return null
  }
  if (url.pathname.includes("/repos/example-org/chaos/")) return "chaos"
  const repo = url.pathname.match(/\/repos\/[^/]+\/repo-([0-9]+)\/issues$/)
  if (repo) return `repo-${repo[1]}`
  if (/\/search\/jql\/?$/.test(url.pathname)) {
    const project = (url.searchParams.get("jql") ?? "").match(/project\s*=\s*([A-Za-z][A-Za-z0-9_]*)/i)
    if (project) return project[1]
  }
  return null
}

/** The same identity for a MonitorEvent's source object. */
export function keyForSource(source) {
  if (!source || typeof source !== "object") return null
  if (typeof source.repo === "string") {
    if (source.repo === "example-org/chaos") return "chaos"
    const repo = source.repo.match(/^example-org\/repo-([0-9]+)$/)
    if (repo) return `repo-${repo[1]}`
  }
  if (typeof source.jql === "string") {
    const project = source.jql.match(/project\s*=\s*([A-Za-z][A-Za-z0-9_]*)/i)
    if (project) return project[1]
  }
  return null
}

/** POST one JSON-RPC method to the stall's HTTP API and resolve its result. */
export function rpcCall({ port, method, params, timeoutMs = 10_000, agent }) {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify({ id: 1, method, params })
    const request = http.request({
      host: "127.0.0.1",
      port,
      path: "/rpc",
      method: "POST",
      agent,
      headers: { "content-type": "application/json", "content-length": Buffer.byteLength(body) },
    }, (response) => {
      let text = ""
      response.setEncoding("utf8")
      response.on("data", (chunk) => { text += chunk })
      response.on("end", () => {
        let message
        try {
          message = JSON.parse(text)
        } catch {
          reject(new Error(`${method}: HTTP ${response.statusCode} with non-JSON body ${JSON.stringify(text.slice(0, 200))}`))
          return
        }
        if (message.error !== undefined) {
          reject(new Error(`${method}: ${message.error}`))
          return
        }
        if (response.statusCode !== 200) {
          reject(new Error(`${method}: HTTP ${response.statusCode} ${JSON.stringify(text.slice(0, 200))}`))
          return
        }
        resolve(message.result)
      })
    })
    request.setTimeout(timeoutMs, () => request.destroy(new Error(`${method} timed out after ${timeoutMs} ms`)))
    request.on("error", reject)
    request.end(body)
  })
}
