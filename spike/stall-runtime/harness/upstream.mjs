// Mock upstream for the stall spike. It runs as its own process so neither candidate scores
// the harness's JSON work, and it returns realistic 100-item payloads with one sliding
// window per repo or project, so every poll after priming sees exactly one new item and one
// that vanished. Logs nothing per request.
import { spawn } from "node:child_process"
import http from "node:http"
import { fileURLToPath, pathToFileURL } from "node:url"
import { stderrCollector } from "./proc.mjs"

const WORDS = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua enim ad minim veniam quis nostrud exercitation ullamco laboris nisi aliquip ex ea commodo consequat".split(" ")
const PAGE_SIZE = 100
const LOREM_LENGTH = 200
const STATE = { repos: new Map(), projects: new Map() }
const START_TIMEOUT_MS = 10_000
let lorem = ""

function loremBody(seed) {
  let state = seed >>> 0
  let text = ""
  while (text.length < LOREM_LENGTH) {
    state = (state * 1664525 + 1013904223) >>> 0
    text += text ? ` ${WORDS[state % WORDS.length]}` : WORDS[state % WORDS.length]
  }
  return text.slice(0, LOREM_LENGTH)
}

function githubItem(org, repo, number) {
  return `{"number":${number},"title":"${repo} issue ${number}","html_url":"https://github.com/${org}/${repo}/issues/${number}","state":"open","labels":[{"name":"atelier:build"}],"body":"${lorem}","user":{"login":"spike"}}`
}

function pullRequestItem(org, repo) {
  return `{"number":1,"title":"${repo} spike pull request (filtered)","html_url":"https://github.com/${org}/${repo}/pull/1","state":"open","labels":[],"body":"filtered","user":{"login":"spike"},"pull_request":{"url":"https://api.github.com/repos/${org}/${repo}/pulls/1","html_url":"https://github.com/${org}/${repo}/pull/1","merged_at":null}}`
}

function githubList(org, repo, top) {
  const items = [pullRequestItem(org, repo)]
  for (let number = top - PAGE_SIZE + 1; number <= top; number += 1) items.push(githubItem(org, repo, number))
  return `[${items.join(",")}]`
}

function jiraList(project, top) {
  const issues = []
  for (let number = top - PAGE_SIZE + 1; number <= top; number += 1) {
    issues.push(`{"expand":"","id":"${number}","self":"https://example.atlassian.net/rest/api/3/issue/${number}","key":"${project}-${number}","fields":{"summary":"${project} issue ${number}"}}`)
  }
  return `{"issues":[${issues.join(",")}],"isLast":true}`
}

function handleGithub(mode, request, response, org, repo) {
  const key = `${org}/${repo}`
  let record = STATE.repos.get(key)
  if (!record) {
    record = { top: PAGE_SIZE - 1, stable: null, etag: `"e-${repo}-stable"` }
    STATE.repos.set(key, record)
  }

  if (mode === "stable") {
    if (request.headers["if-none-match"] === record.etag) return send(response, 304, null, { etag: record.etag })
    record.stable ??= githubList(org, repo, PAGE_SIZE)
    return send(response, 200, record.stable, { etag: record.etag })
  }

  record.top += 1
  send(response, 200, githubList(org, repo, record.top), { etag: `"e-${repo}-${record.top}"` })
}

function handleJira(mode, response, url) {
  const jql = url.searchParams.get("jql") ?? ""
  const match = jql.match(/project\s*=\s*([A-Za-z][A-Za-z0-9_]*)/i)
  if (!match) return send(response, 400, JSON.stringify({ error: "missing project in jql" }))
  const project = match[1]
  let record = STATE.projects.get(project)
  if (!record) {
    record = { top: PAGE_SIZE - 1, stable: null }
    STATE.projects.set(project, record)
  }

  if (mode === "stable") {
    record.stable ??= jiraList(project, PAGE_SIZE)
    return send(response, 200, record.stable)
  }
  record.top += 1
  send(response, 200, jiraList(project, record.top))
}

function send(response, status, body, headers = {}) {
  const payload = body === null ? null : Buffer.from(body)
  const head = { "content-type": "application/json", ...headers }
  if (payload) head["content-length"] = payload.length
  response.writeHead(status, head)
  response.end(payload ?? undefined)
}

/** Starts the mock upstream as its own process and resolves once it prints its port. */
export function spawnUpstream({ mode = "changing", seed = 1 } = {}) {
  if (mode !== "changing" && mode !== "stable") throw new Error(`spawnUpstream: invalid mode ${mode}`)
  if (!Number.isInteger(seed)) throw new Error(`spawnUpstream: invalid seed ${seed}`)
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), "--port", "0", "--mode", mode, "--seed", String(seed)], { stdio: ["ignore", "pipe", "pipe"] })
  const stderr = stderrCollector(child.stderr)
  let buffer = ""
  let settled = false
  let timer = null
  let resolveRun
  let rejectRun
  const ready = new Promise((resolve, reject) => {
    resolveRun = resolve
    rejectRun = reject
  })
  const fail = (message) => {
    if (settled) return
    settled = true
    clearTimeout(timer)
    child.kill("SIGKILL")
    rejectRun(new Error(message))
  }
  timer = setTimeout(() => fail(`mock upstream printed no port within 10 s: ${stderr.tail().join(" | ")}`), START_TIMEOUT_MS)
  timer.unref()

  child.stdout.setEncoding("utf8")
  child.stdout.on("data", (chunk) => {
    if (settled) return
    buffer += chunk
    let newline = buffer.indexOf("\n")
    while (newline !== -1) {
      const line = buffer.slice(0, newline).trim()
      buffer = buffer.slice(newline + 1)
      newline = buffer.indexOf("\n")
      if (!line || settled) continue
      let message
      try {
        message = JSON.parse(line)
      } catch {
        fail(`mock upstream printed a non-JSON line: ${line}`)
        return
      }
      if (!message || !Number.isInteger(message.port)) {
        fail(`mock upstream printed no port: ${line}`)
        return
      }
      settled = true
      clearTimeout(timer)
      resolveRun({ child, port: message.port, stderr })
    }
  })
  child.once("error", (error) => fail(`mock upstream failed to start: ${error.message}`))
  child.once("exit", (code) => fail(`mock upstream exited with code ${code} before reporting a port`))
  return ready
}

function parseArgs(argv) {
  const options = { port: 0, mode: "changing", seed: 1 }
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    const value = argv[index + 1]
    if (arg === "--port") { options.port = Number(value); index += 1 }
    else if (arg === "--mode") { options.mode = value; index += 1 }
    else if (arg === "--seed") { options.seed = Number(value); index += 1 }
    else throw new Error(`unknown argument ${arg}`)
  }
  if (!Number.isInteger(options.port) || options.port < 0 || options.port > 65_535) throw new Error(`invalid port ${options.port}`)
  if (options.mode !== "changing" && options.mode !== "stable") throw new Error(`invalid mode ${options.mode}`)
  if (!Number.isInteger(options.seed)) throw new Error(`invalid seed ${options.seed}`)
  return options
}

function main() {
  const options = parseArgs(process.argv.slice(2))
  lorem = loremBody(options.seed)

  const server = http.createServer((request, response) => {
    const url = new URL(request.url, "http://upstream")
    if (request.method !== "GET") return send(response, 405, JSON.stringify({ error: "method not allowed" }))

    const github = url.pathname.match(/^\/repos\/([^/]+)\/([^/]+)\/issues$/)
    if (github) return handleGithub(options.mode, request, response, github[1], github[2])
    if (url.pathname === "/rest/api/3/search/jql") return handleJira(options.mode, response, url)
    send(response, 404, JSON.stringify({ error: "not found" }))
  })
  server.keepAliveTimeout = 65_000

  server.on("error", (error) => {
    process.stderr.write(`mock upstream failed: ${error.message}\n`)
    process.exitCode = 1
  })
  server.listen(options.port, "127.0.0.1", () => {
    process.stdout.write(`${JSON.stringify({ port: server.address().port })}\n`)
  })
}

if (process.argv[1] && pathToFileURL(process.argv[1]).href === import.meta.url) main()
