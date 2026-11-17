// Only @sourcefed/core's default state directory reads the OS, and a plugin realm never
// builds a store, so this stub exists to satisfy the import rather than to work.

function unavailable() {
  throw new Error("os access is unavailable inside a plugin realm")
}

export default { homedir: unavailable, tmpdir: unavailable }
