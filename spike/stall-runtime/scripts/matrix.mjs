#!/usr/bin/env node
// Measure every requested variant of one scenario, one timed harness run at a time.
// Variants are interleaved by run index so runner drift hits them all alike.
//
// Usage (from spike/stall-runtime):
//   node scripts/matrix.mjs --variants ts,rust-seq,rust-realm,rust-djs --scenario steady \
//     --monitors 100 --runs 10 --interval 15 --window 45 --out results/steady-100
// --interval is the monitor poll interval in seconds, passed to every run.

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const HARNESS = join(ROOT, 'harness/run.mjs');
const PLUGINS = join(ROOT, 'plugins-dist');
const RUST_BINARY = join(ROOT, 'rust/target/release/stall-b');
const RUST_DJS_BINARY = join(ROOT, 'rust/target/release/stall-c');
const OUTPUT_LIMIT = 256 * 1024;
const TIMEOUT_SLACK_MS = 180_000;

const VARIANTS = {
  ts: { candidate: 'ts', scheduler: null, stallCmd: join(ROOT, '.vendor/sourcefed/.spike/stall-a.mjs') },
  'rust-seq': { candidate: 'rust', scheduler: 'seq', stallCmd: RUST_BINARY },
  'rust-realm': { candidate: 'rust', scheduler: 'realm', stallCmd: RUST_BINARY },
  // The same Rust stall twice: stall-b builds the plugin realms in QuickJS, stall-c in djs.
  // Only stall-c is told which engine to use, because stall-b's engine is compiled in.
  'rust-djs': {
    candidate: 'rust', scheduler: 'realm', stallCmd: RUST_DJS_BINARY, engine: 'djs',
    build: 'cargo build --release --no-default-features --features engine-djs --manifest-path rust/Cargo.toml, then copy rust/target/release/stall-b to rust/target/release/stall-c',
  },
};

const DEFAULTS = {
  variants: 'ts,rust-seq,rust-realm', scenario: undefined, monitors: undefined, runs: undefined,
  window: undefined, interval: '15', chaos: 'none', out: undefined, node: process.execPath, env: [],
};

function fail(message) {
  console.error(`matrix: ${message}`);
  process.exit(2);
}

function parseArgs(argv) {
  const opts = { ...DEFAULTS };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (!arg.startsWith('--')) fail(`unexpected argument "${arg}"`);
    const key = arg.slice(2);
    if (!(key in opts)) fail(`unknown option "${arg}"`);
    const value = argv[(index += 1)];
    if (value === undefined) fail(`${arg} needs a value`);
    if (key === 'env') opts.env.push(value);
    else opts[key] = value;
  }
  return opts;
}

function integer(value, flag, minimum) {
  const number = Number(value);
  if (!Number.isInteger(number) || number < minimum) fail(`${flag} must be an integer >= ${minimum}`);
  return number;
}

function configure(opts) {
  for (const flag of ['scenario', 'monitors', 'runs', 'window']) if (opts[flag] === undefined) fail(`--${flag} is required`);
  const variants = [...new Set(opts.variants.split(',').map((name) => name.trim()).filter(Boolean))];
  if (variants.length === 0) fail('--variants needs at least one variant');
  for (const variant of variants) if (!(variant in VARIANTS)) fail(`unknown variant "${variant}"`);
  if (!['none', 'throw', 'loop'].includes(opts.chaos)) fail('--chaos must be none, throw or loop');
  const out = opts.out ?? `results/matrix-${opts.scenario}-${opts.monitors}`;
  const cfg = {
    scenario: opts.scenario,
    monitors: integer(opts.monitors, '--monitors', 0),
    runs: integer(opts.runs, '--runs', 1),
    window: integer(opts.window, '--window', 1),
    interval: integer(opts.interval, '--interval', 1),
    chaos: opts.chaos,
    node: opts.node,
    env: opts.env,
    variants,
    outDir: isAbsolute(out) ? out : join(ROOT, out),
  };
  mkdirSync(cfg.outDir, { recursive: true });
  return cfg;
}

function invocation(variant, run, cfg) {
  const spec = VARIANTS[variant];
  const out = join(cfg.outDir, `${variant}-${cfg.scenario}-${cfg.monitors}-${run}.json`);
  const args = [
    HARNESS, '--candidate', spec.candidate, '--scenario', cfg.scenario,
    '--monitors', String(cfg.monitors), '--interval', String(cfg.interval),
    '--window', String(cfg.window), '--chaos', cfg.chaos, '--run-index', String(run),
    '--out', out, '--stall-cmd', spec.stallCmd,
  ];
  if (spec.candidate === 'ts') {
    args.push('--node');
  } else {
    args.push('--scheduler', spec.scheduler);
    args.push('--env', `STALL_PLUGIN_DIR=${PLUGINS}`);
    args.push('--env', `STALL_SCHEDULER=${spec.scheduler}`);
    if (spec.engine) args.push('--env', `STALL_ENGINE=${spec.engine}`);
  }
  for (const pair of cfg.env) args.push('--env', pair);
  return { args, out };
}

function keep(current, chunk) {
  return (current + chunk.toString()).slice(-OUTPUT_LIMIT);
}

function execute(nodeBin, args, timeoutMs) {
  return new Promise((done) => {
    const child = spawn(nodeBin, args, { cwd: ROOT, env: process.env, stdio: ['ignore', 'ignore', 'pipe'] });
    let stderr = '';
    let timedOut = false;
    child.stderr.on('data', (chunk) => { stderr = keep(stderr, chunk); });
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGKILL');
    }, timeoutMs);
    const finish = (code, error) => {
      clearTimeout(timer);
      done({ code, timedOut, stderr: error ? `${stderr}\n${error.message}` : stderr });
    };
    child.on('error', (error) => finish(null, error));
    child.on('close', (code) => finish(code));
  });
}

function quote(value) {
  return /^[\w./:=+-]+$/.test(value) ? value : `'${value.replaceAll("'", "'\\''")}'`;
}

function describe(cfg, variant, run, out, ok, seconds, result) {
  const status = ok ? 'ok  ' : 'FAIL';
  const note = result.timedOut ? ' (timed out)' : '';
  return `${status} ${variant} ${cfg.scenario} ${cfg.monitors}m run ${run}/${cfg.runs} in ${seconds.toFixed(1)}s -> ${relative(ROOT, out)}${note}`;
}

async function main() {
  const cfg = configure(parseArgs(process.argv.slice(2)));
  if (!existsSync(HARNESS)) fail(`harness not found at ${HARNESS}`);
  for (const variant of cfg.variants) {
    const spec = VARIANTS[variant];
    if (!existsSync(spec.stallCmd)) {
      const hint = spec.build ? `; build it with ${spec.build}` : '';
      fail(`${variant}: stall command not found at ${spec.stallCmd}${hint}`);
    }
    if (spec.candidate === 'rust' && !existsSync(PLUGINS)) fail(`rust: plugin bundles not found at ${PLUGINS}`);
  }

  const streaks = new Map(cfg.variants.map((variant) => [variant, 0]));
  const successes = new Map(cfg.variants.map((variant) => [variant, 0]));
  const failedCommands = [];
  const started = Date.now();
  let aborted = false, okCount = 0, failedCount = 0, attempted = 0;

  for (let run = 1; run <= cfg.runs && !aborted; run += 1) {
    for (const variant of cfg.variants) {
      if (aborted) break;
      attempted += 1;
      const { args, out } = invocation(variant, run, cfg);
      rmSync(out, { force: true });
      const began = Date.now();
      const result = await execute(cfg.node, args, cfg.window * 1000 + TIMEOUT_SLACK_MS);
      const seconds = (Date.now() - began) / 1000;
      const ok = result.code === 0 && !result.timedOut && existsSync(out);
      console.log(describe(cfg, variant, run, out, ok, seconds, result));

      if (ok) {
        okCount += 1;
        successes.set(variant, successes.get(variant) + 1);
        streaks.set(variant, 0);
      } else {
        failedCount += 1;
        const command = [cfg.node, ...args].map(quote).join(' ');
        failedCommands.push(command);
        const streak = streaks.get(variant) + 1;
        streaks.set(variant, streak);
        console.log(`     command: ${command}`);
        if (result.stderr.trim()) console.log(`     stderr: ${result.stderr.trim().split('\n').slice(-5).join('\n       ')}`);
        if (streak >= 2) {
          console.error(`matrix: aborting after ${variant} failed twice in a row`);
          aborted = true;
        }
      }
    }
  }

  const minutes = (Date.now() - started) / 60000;
  console.log(`matrix: ${okCount} ok, ${failedCount} failed, ${attempted}/${cfg.runs * cfg.variants.length} attempted in ${minutes.toFixed(1)} min`);
  if (failedCommands.length > 0) {
    console.log('matrix: failed commands:');
    for (const command of failedCommands) console.log(`  ${command}`);
  }
  const empty = cfg.variants.filter((variant) => successes.get(variant) === 0);
  if (aborted || empty.length > 0) {
    if (empty.length > 0) console.error(`matrix: no successful run for ${empty.join(', ')}`);
    process.exit(1);
  }
}

await main();
