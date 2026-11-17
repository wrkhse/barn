#!/usr/bin/env node
import { build } from "esbuild"
import { createHash } from "node:crypto"
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs"
import path from "node:path"
import { fileURLToPath } from "node:url"

const ROOT = fileURLToPath(new URL("..", import.meta.url))
const VENDOR = path.join(ROOT, ".vendor", "sourcefed")
const OUT_DIR = path.join(ROOT, "plugins-dist")
const SHIM_DIR = path.join(ROOT, "ts", "shims")

// The entries import the pinned provider modules through synthetic specifiers so they never
// depend on a relative path into .vendor, which the vendor script may recreate.
const PINNED = new Map([
  ["#pinned/github-issues", path.join(VENDOR, "providers", "github", "src", "issues", "poll.ts")],
  ["#pinned/jira-query", path.join(VENDOR, "providers", "jira", "src", "query", "poll.ts")],
])

const PLUGINS = [
  { type: "github-issues", entry: path.join(ROOT, "ts", "plugin-github-issues.mjs"), pin: "#pinned/github-issues" },
  { type: "jira-query", entry: path.join(ROOT, "ts", "plugin-jira-query.mjs"), pin: "#pinned/jira-query" },
]

// A realm has no Node built-ins. Only the storage module's imports are reachable from the
// provider graph, and an unexpected one is a build error rather than a silent runtime hole.
const NODE_STUBS = new Map([
  ["node:fs/promises", path.join(SHIM_DIR, "node-fs.mjs")],
  ["node:os", path.join(SHIM_DIR, "node-os.mjs")],
  ["node:path", path.join(SHIM_DIR, "node-path.mjs")],
])

try {
  await main()
} catch (error) {
  console.error(`[plugins] ${messageOf(error)}`)
  process.exitCode = 1
}

async function main() {
  if (!existsSync(VENDOR)) throw new Error(`${VENDOR} is missing; run npm run vendor first`)
  for (const plugin of PLUGINS) {
    const pinned = PINNED.get(plugin.pin)
    if (!pinned || !existsSync(pinned)) throw new Error(`${pinned ?? plugin.pin} is missing; run npm run vendor first`)
  }

  mkdirSync(OUT_DIR, { recursive: true })
  const manifest = {}
  for (const plugin of PLUGINS) {
    const outfile = path.join(OUT_DIR, `${plugin.type}.js`)
    await build({
      entryPoints: [plugin.entry],
      outfile,
      bundle: true,
      format: "iife",
      platform: "neutral",
      target: "es2022",
      // Neutral leaves the package fields empty, but the bundle still needs the ESM entries
      // of valibot and the workspace packages.
      mainFields: ["module", "main"],
      conditions: ["import", "module", "default"],
      external: [],
      absWorkingDir: ROOT,
      plugins: [stallPlugin()],
      logLevel: "warning",
    })

    const code = readFileSync(outfile, "utf8")
    verify(plugin.type, code)
    manifest[plugin.type] = {
      file: outfile,
      bytes: Buffer.byteLength(code),
      sha256: createHash("sha256").update(code).digest("hex"),
    }
  }

  writeFileSync(path.join(OUT_DIR, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`)
  console.log(JSON.stringify(manifest))
}

function stallPlugin() {
  return {
    name: "stall",
    setup(builder) {
      builder.onResolve({ filter: /^#pinned\// }, (args) => {
        const resolved = PINNED.get(args.path)
        if (!resolved) return { errors: [{ text: `unknown pinned provider import ${args.path}` }] }
        if (!existsSync(resolved)) return { errors: [{ text: `${resolved} is missing; run npm run vendor first` }] }
        return { path: resolved }
      })

      builder.onResolve({ filter: /^node:/ }, (args) => {
        const shim = NODE_STUBS.get(args.path)
        if (!shim) return { errors: [{ text: `${args.path} is not available to a plugin realm; add a stub in ts/shims/` }] }
        return { path: shim }
      })
    },
  }
}

// The realm evaluates one plain script: a Node built-in import or a CommonJS require would
// fail there, and a bundle that forgot its assignment would load without a poll entry point.
function verify(type, code) {
  const offenders = []
  if (code.includes("node:")) offenders.push('a "node:" specifier')
  if (code.includes("require(")) offenders.push('a "require(" call')
  if (!code.includes("__stall.pluginPoll")) offenders.push("no globalThis.__stall.pluginPoll assignment")
  if (offenders.length > 0) throw new Error(`${type}.js contains ${offenders.join(" and ")}`)
}

function messageOf(error) {
  return error instanceof Error ? error.message : String(error)
}
