import { Socket } from "node:net"

const MAX_LINE_BYTES = 1024 * 1024
const REQUEST_TIMEOUT_MS = 30_000
// The contract allows five seconds from lane.drain to exit; this bounds the wait for the
// kernel to take the reply if the peer stops reading.
const FLUSH_TIMEOUT_MS = 1_000

/**
 * The fd 3 lane: newline-delimited JSON-RPC 2.0 over the inherited socket. Methods in
 * `handlers` answer the harness, `net.request` arrives through the same map, and a reply
 * for `lane.drain` ends the lane once it is flushed.
 */
export function openLane({ handlers = {}, onClose } = {}) {
  const fd = Number(process.env.STALL_LANE_FD ?? 3)
  if (!Number.isInteger(fd) || fd < 3) throw new Error(`STALL_LANE_FD must be a file descriptor of 3 or more, got ${process.env.STALL_LANE_FD ?? fd}`)

  const socket = new Socket({ fd, readable: true, writable: true })
  socket.setNoDelay(true)

  const pending = new Map()
  let tail = Buffer.alloc(0)
  let nextID = 1
  let closed = false
  let ending = false
  let exitTimer

  socket.on("data", (chunk) => receive(chunk))
  socket.on("error", (error) => shutdown({ drained: false, error }))
  socket.on("end", () => shutdown({ drained: false }))
  socket.on("close", () => shutdown({ drained: false }))

  return { request, sendLog }

  function request(method, params = {}) {
    if (closed) return Promise.reject(new Error("the lane is closed"))
    const id = nextID++
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id)
        reject(new Error(`lane request ${method} timed out after ${REQUEST_TIMEOUT_MS} ms`))
      }, REQUEST_TIMEOUT_MS)
      // A pending reply must not keep the process alive after drain.
      timer.unref?.()
      pending.set(id, { resolve, reject, timer })
      try {
        send({ jsonrpc: "2.0", id, method, params })
      } catch (error) {
        pending.delete(id)
        clearTimeout(timer)
        reject(error)
      }
    })
  }

  function sendLog(level, message) {
    try {
      send({ jsonrpc: "2.0", method: "log", params: { level, message } })
    } catch {
      // A log is best effort; losing it must not break the poll that wrote it.
    }
  }

  function receive(chunk) {
    tail = tail.length === 0 ? chunk : Buffer.concat([tail, chunk])
    let newline = tail.indexOf(0x0a)

    while (newline !== -1) {
      const line = tail.subarray(0, newline).toString("utf8")
      tail = tail.subarray(newline + 1)
      if (line.trim() !== "") accept(line)
      newline = tail.indexOf(0x0a)
    }

    if (tail.length > MAX_LINE_BYTES) fatal(new Error(`a lane line passed ${MAX_LINE_BYTES} bytes without a newline`))
  }

  function accept(line) {
    let message
    try {
      message = JSON.parse(line)
    } catch {
      fatal(new Error("the lane received a line that is not JSON"))
      return
    }
    if (!message || typeof message !== "object" || Array.isArray(message)) {
      fatal(new Error("the lane received a message that is not an object"))
      return
    }

    if (typeof message.method !== "string") {
      settle(message)
      return
    }
    if (message.id === undefined) {
      if (message.method === "log") announce(message.params)
      else process.stderr.write(`[stall] ignored lane notification ${message.method}\n`)
      return
    }
    void dispatch(message).catch((error) => fatal(error))
  }

  function settle(message) {
    const entry = pending.get(message.id)
    if (!entry) return
    pending.delete(message.id)
    clearTimeout(entry.timer)
    if (message.error) {
      const error = new Error(`lane error ${message.error.code}: ${message.error.message}`)
      error.code = message.error.code
      entry.reject(error)
      return
    }
    entry.resolve(message.result)
  }

  async function dispatch(message) {
    const handler = handlers[message.method]
    try {
      if (!handler) throw new Error(`unknown lane method ${message.method}`)
      const result = await handler(message.params ?? {})
      send({ jsonrpc: "2.0", id: message.id, result: result ?? null })
    } catch (error) {
      send({ jsonrpc: "2.0", id: message.id, error: { code: -32001, message: messageOf(error) } })
    } finally {
      if (message.method === "lane.drain") endLane()
    }
  }

  function send(message) {
    if (closed || ending) throw new Error("the lane is closed")
    const json = JSON.stringify(message)
    if (Buffer.byteLength(json) > MAX_LINE_BYTES) throw new Error("a lane message exceeds 1 MiB")
    socket.write(`${json}\n`)
  }

  function endLane() {
    if (ending || closed) return
    ending = true
    exitTimer = setTimeout(() => shutdown({ drained: true }), FLUSH_TIMEOUT_MS)
    socket.end(() => shutdown({ drained: true }))
  }

  function shutdown(info) {
    if (closed) return
    closed = true
    if (exitTimer) clearTimeout(exitTimer)
    const error = info.error ?? new Error("the lane closed")
    for (const entry of pending.values()) {
      clearTimeout(entry.timer)
      entry.reject(error)
    }
    pending.clear()
    socket.destroy()
    onClose?.(info)
  }

  function fatal(error) {
    process.stderr.write(`[stall] lane protocol error: ${messageOf(error)}\n`)
    shutdown({ drained: false, error, fatal: true })
  }

  function announce(params) {
    const level = typeof params?.level === "string" ? params.level : "info"
    const message = typeof params?.message === "string" ? params.message : ""
    process.stderr.write(`[stall] ${level}: ${message}\n`)
  }
}

function messageOf(error) {
  return error instanceof Error ? error.message : String(error)
}
