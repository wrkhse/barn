// The fake host. It holds the /events stream, timestamps every frame on the harness clock,
// and acknowledges delivered events in batches so the stall's queue stays bounded. It
// reconnects once if the stream ends early, because an at-least-once queue can survive one
// lost connection.
import http from "node:http"
import { rpcCall } from "./broker.mjs"

const CONNECT_TIMEOUT_MS = 5000
const RECONNECT_DELAY_MS = 500
const MAX_BUFFER_CHARS = 4 * 1024 * 1024
const MAX_PENDING_IDS = 20_000
const FLUSH_AT_IDS = 5000

/**
 * Feeds SSE text chunks and calls onFrame({tNs, type, events}) on each complete frame.
 * Comment lines are counted and ignored; data lines are joined with "\n" per the SSE spec.
 */
export function createFrameParser({ stats, onFrame }) {
  let buffer = ""
  let dataLines = []

  const dispatch = () => {
    if (dataLines.length === 0) return
    const text = dataLines.join("\n")
    dataLines = []
    let message
    try {
      message = JSON.parse(text)
    } catch {
      stats.parseErrors += 1
      return
    }
    stats.frames += 1
    onFrame({ tNs: process.hrtime.bigint(), type: String(message.type ?? ""), events: Array.isArray(message.events) ? message.events : [] })
  }

  const handleLine = (line) => {
    if (line === "") return dispatch()
    if (line.startsWith(":")) {
      stats.comments += 1
      return
    }
    if (line.startsWith("data:")) dataLines.push(line.slice(5).replace(/^ /, ""))
  }

  return {
    push(chunk) {
      buffer += chunk
      let newline = buffer.indexOf("\n")
      while (newline !== -1) {
        handleLine(buffer.slice(0, newline).replace(/\r$/, ""))
        buffer = buffer.slice(newline + 1)
        newline = buffer.indexOf("\n")
      }
      if (buffer.length > MAX_BUFFER_CHARS) {
        stats.parseErrors += 1
        buffer = ""
      }
    },
    reset() {
      buffer = ""
      dataLines = []
    },
  }
}

export function startSseHost({ port, target, ackBatchMs = 2000, onFrame }) {
  const stats = {
    frames: 0,
    events: 0,
    parseErrors: 0,
    comments: 0,
    reconnects: 0,
    connectErrors: 0,
    ackBatches: 0,
    ackedEvents: 0,
    ackFailures: 0,
    droppedIDs: 0,
    endedEarly: false,
    subscribed: false,
  }
  const pendingIDs = new Set()
  const encoded = Buffer.from(JSON.stringify(target)).toString("base64url")
  let stopped = false
  let reconnected = false
  let request = null
  let response = null
  let reconnectTimer = null
  let ackTimer = null
  let flushing = false
  let readyResolve
  let readyReject
  const ready = new Promise((resolve, reject) => {
    readyResolve = resolve
    readyReject = reject
  })
  ready.catch(() => {})

  const readyTimer = setTimeout(() => {
    readyReject(new Error(`SSE host received no subscribed frame within ${CONNECT_TIMEOUT_MS} ms`))
  }, CONNECT_TIMEOUT_MS)
  readyTimer.unref()

  const parser = createFrameParser({ stats, onFrame: handleFrame })

  function handleFrame(frame) {
    if (frame.type === "subscribed") {
      stats.subscribed = true
      clearTimeout(readyTimer)
      readyResolve()
    }
    if (frame.type === "event") {
      stats.events += frame.events.length
      for (const event of frame.events) {
        if (!event || typeof event.id !== "string") continue
        if (pendingIDs.has(event.id)) continue
        if (pendingIDs.size >= MAX_PENDING_IDS) {
          stats.droppedIDs += 1
          continue
        }
        pendingIDs.add(event.id)
      }
      if (pendingIDs.size >= FLUSH_AT_IDS) flushAcks()
    }
    if (onFrame) onFrame(frame)
  }

  function connect() {
    if (stopped) return
    parser.reset()
    let settled = false
    const ended = () => {
      if (settled || stopped) return
      settled = true
      scheduleReconnect()
    }

    request = http.get({
      host: "127.0.0.1",
      port,
      path: `/events?target=${encoded}`,
      agent: false,
      headers: { accept: "text/event-stream" },
    }, (incoming) => {
      if (incoming.statusCode !== 200) {
        stats.connectErrors += 1
        incoming.resume()
        ended()
        return
      }
      response = incoming
      incoming.setEncoding("utf8")
      incoming.on("data", (chunk) => parser.push(chunk))
      incoming.on("end", ended)
      incoming.on("error", ended)
    })
    request.on("error", ended)
  }

  function scheduleReconnect() {
    if (stopped) return
    request = null
    response = null
    if (reconnected) {
      stats.endedEarly = true
      return
    }
    reconnected = true
    stats.reconnects += 1
    reconnectTimer = setTimeout(connect, RECONNECT_DELAY_MS)
    reconnectTimer.unref()
  }

  function flushAcks() {
    if (flushing || pendingIDs.size === 0) return Promise.resolve()
    const ids = [...pendingIDs]
    pendingIDs.clear()
    flushing = true
    stats.ackBatches += 1
    return rpcCall({
      port,
      method: "monitor.ack",
      params: { target, eventIDs: ids },
      timeoutMs: Math.max(2000, ackBatchMs),
    }).then(() => {
      stats.ackedEvents += ids.length
    }).catch(() => {
      stats.ackFailures += 1
      for (const id of ids) {
        if (pendingIDs.size < MAX_PENDING_IDS) pendingIDs.add(id)
        else stats.droppedIDs += 1
      }
    }).finally(() => {
      flushing = false
    })
  }

  ackTimer = setInterval(() => { flushAcks() }, ackBatchMs)
  ackTimer.unref()
  connect()

  async function stop() {
    if (stopped) return
    stopped = true
    clearTimeout(readyTimer)
    clearInterval(ackTimer)
    clearTimeout(reconnectTimer)
    if (!stats.subscribed) readyReject(new Error("SSE host stopped before it subscribed"))
    if (request) request.destroy()
    if (response) response.destroy()
    request = null
    response = null
    await flushAcks()
  }

  return { stop, stats, ready }
}
