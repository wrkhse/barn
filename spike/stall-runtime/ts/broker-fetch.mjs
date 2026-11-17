const JIRA_SEARCH = "/rest/api/3/search/jql"

/**
 * Replaces globalThis.fetch with a broker client. Every provider call in sourcefed goes
 * through fetch, so this is the only outbound path the stall has: the original fetch is
 * never kept, and nothing here imports a network module.
 */
export function installBrokerFetch({ request, faults = [], sendLog = () => {} }) {
  const rules = validateFaults(faults)
  let inflight = 0

  globalThis.fetch = async (input, init = {}) => {
    if (init.signal?.aborted) throw abortReason(init.signal)

    const url = urlOf(input)
    const method = methodOf(input, init)
    const headers = headerRecord(init.headers ?? requestHeadersOf(input))
    const body = bodyText(init.body)

    applyFaults(rules, url, sendLog)

    const plugin = pluginFor(url)
    inflight += 1
    try {
      const reply = await untilAborted(
        request("net.request", { plugin, method, url, headers, ...(body === undefined ? {} : { body }) }),
        init.signal,
      )
      return toResponse(reply)
    } finally {
      inflight -= 1
    }
  }

  return { inflight: () => inflight }
}

function validateFaults(faults) {
  if (!Array.isArray(faults)) throw new Error("faults must be an array")
  return faults.map((fault, index) => {
    if (!fault || typeof fault.match !== "string" || fault.match === "") {
      throw new Error(`faults[${index}].match must be a non-empty string`)
    }
    if (fault.mode !== "throw" && fault.mode !== "loop") {
      throw new Error(`faults[${index}].mode must be "throw" or "loop"`)
    }
    return { match: fault.match, mode: fault.mode }
  })
}

// A fault is applied before any broker traffic, so "throw" fails the poll while its sibling
// monitors keep polling, and "loop" never yields the thread back to the daemon.
function applyFaults(rules, url, sendLog) {
  for (const rule of rules) {
    if (!url.includes(rule.match)) continue
    if (rule.mode === "throw") {
      sendLog("warn", `injected fault: throwing for ${url}`)
      throw new Error("injected fault")
    }
    sendLog("warn", `injected fault: looping synchronously for ${url}`)
    // Deliberate synchronous hang: the supervisor measures whether one stalled poll takes
    // the whole process down when nothing but the process itself can stop it.
    while (true) {}
  }
}

function pluginFor(url) {
  if (url.includes(JIRA_SEARCH)) return "jira-query"
  if (url.includes("/repos/") || url.includes("/search/issues")) return "github-issues"
  throw new Error(`no barn plugin handles ${url}`)
}

function urlOf(input) {
  if (typeof input === "string") return input
  if (input instanceof URL) return input.href
  if (input && typeof input === "object" && typeof input.url === "string") return input.url
  throw new Error("the barn broker fetch needs a URL string or a Request")
}

function methodOf(input, init) {
  const method = init.method ?? (input && typeof input === "object" ? input.method : undefined) ?? "GET"
  return String(method).toUpperCase()
}

function requestHeadersOf(input) {
  return input && typeof input === "object" ? input.headers : undefined
}

function headerRecord(headers) {
  if (!headers) return {}
  if (typeof headers.entries === "function") return Object.fromEntries([...headers.entries()].map(([name, value]) => [name, String(value)]))
  return Object.fromEntries(Object.entries(headers).map(([name, value]) => [name, String(value)]))
}

function bodyText(body) {
  if (body === undefined || body === null) return undefined
  if (typeof body === "string") return body
  if (typeof URLSearchParams !== "undefined" && body instanceof URLSearchParams) return body.toString()
  throw new Error("the barn broker only carries string request bodies")
}

// Aborting before the broker answers rejects the caller's fetch, as undici would; the
// orphaned lane request finishes on its own 30-second timeout.
function untilAborted(brokerCall, signal) {
  if (!signal) return brokerCall
  if (signal.aborted) return Promise.reject(abortReason(signal))

  let onAbort
  const aborted = new Promise((_, reject) => {
    onAbort = () => reject(abortReason(signal))
    signal.addEventListener("abort", onAbort, { once: true })
  })
  return Promise.race([brokerCall, aborted]).finally(() => signal.removeEventListener("abort", onAbort))
}

function abortReason(signal) {
  return signal.reason ?? new Error("the request was aborted")
}

function toResponse(reply) {
  if (!reply || typeof reply !== "object") throw new Error("the barn broker returned no response")
  const status = Number(reply.status)
  if (!Number.isFinite(status)) throw new Error("the barn broker returned no status")

  const text = typeof reply.body === "string" ? reply.body : ""
  const headers = headerBag(reply.headers)

  return {
    status,
    ok: status >= 200 && status < 300,
    headers,
    text: async () => text,
    json: async () => JSON.parse(text),
    clone: () => toResponse(reply),
    // Only GitHub job logs stream a body, and this workload never fetches one.
    get body() {
      return {
        getReader() {
          throw new Error("streaming response bodies are unavailable through the barn broker")
        },
      }
    },
  }
}

function headerBag(raw) {
  const values = new Map()
  for (const [name, value] of Object.entries(raw ?? {})) {
    values.set(name.toLowerCase(), Array.isArray(value) ? value.join(", ") : String(value))
  }
  return {
    get: (name) => values.get(String(name).toLowerCase()) ?? null,
  }
}
