// @sourcefed/core imports node:path for its state directory. The plugin realm passes a
// source and a cursor, never a path, so these stubs only need to keep the import resolvable.

function unavailable() {
  throw new Error("path access is unavailable inside a plugin realm")
}

export default { join: unavailable, resolve: unavailable, dirname: unavailable, sep: "/" }
