# barn-luau

Embed a stall's Luau plugin in a Rust host. Each `Sandbox` loads
`plugin.luau` into its own VM and exposes the plugin's hooks, named runnables,
and manifest.

## Run a hook and a runnable

This plugin denies a request when the host context marks it read-only. Its
`status` runnable returns a JSON object.

```rust
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use barn_luau::api::{Invocation, Outcome};
use barn_luau::{Capabilities, Clock, Limits, Sandbox};
use serde_json::json;

static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct HostClock(AtomicU64);

impl Clock for HostClock {
    fn micros(&self) -> u128 {
        u128::from(self.0.fetch_add(1, Ordering::Relaxed))
    }
}

struct PluginDir(PathBuf);

impl Drop for PluginDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prefix = format!("luau-{}-", std::process::id());
    let path = loop {
        let candidate = std::env::temp_dir().join(format!(
            "{prefix}{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let plugin = PluginDir(path);
    fs::write(
        plugin.0.join("plugin.luau"),
        r#"
        return {
            name = "storage",
            beforeWrite = function(payload, ctx)
                if ctx.readOnly then
                    return ctx.deny("read-only host")
                end
            end,
            tools = {
                status = {
                    run = function() return { ready = true } end,
                },
            },
        }
        "#,
    )?;

    let limits = Limits::new(16 * 1024 * 1024, 100)?;
    let clock = Arc::new(HostClock(AtomicU64::new(0)));
    let sandbox = Sandbox::new(&plugin.0, limits, Capabilities::default(), clock)?;

    let invocation = Invocation {
        hook: "beforeWrite".into(),
        payload: json!({ "key": "notes" }),
        context: json!({ "readOnly": true }),
    };
    let outcome = sandbox.hook(&invocation, 50)?;
    assert_eq!(outcome, Outcome::Deny { reason: "read-only host".into() });

    let status = sandbox.run("status", &json!({}), 50)?;
    assert_eq!(status, json!({ "ready": true }));

    Ok(())
}
```

The host owns the clock: it supplies an `Arc<dyn Clock>`, and barn-luau
reads no wall clock. `Clock::micros` returns the host's monotonic microsecond
count; this example advances a counter so it stays deterministic.

Hooks take `(payload, ctx)`. The fields of a host context object appear
directly on `ctx`; another value appears as `ctx.host`. Verdict helpers
and registered capabilities take precedence over host fields. A hook
returns `nil` to allow the call, or one verdict built with a helper:
`ctx.deny(reason)` refuses it, `ctx.rewrite(payload)` replaces the whole
payload, `ctx.annotate(fields)` attaches JSON object fields (explicit
nulls survive), and `ctx.drop()` removes the call. A result with more than
one verdict is refused.

The manifest contains the plugin's `name`, `needs`, and a `tools` array.
Tool entries carry their name and other non-function fields.

## Confinement and limits

- `require` resolves from the calling module's folder and only loads modules
  within the plugin folder. It refuses absolute paths, parent components,
  escaping symlinks, and cycles. It also refuses any regular file with more
  than one link, with a named reason: std cannot enumerate every link, so even
  a hard link whose other paths are inside the folder is refused.
- The host sets a Luau allocator ceiling of at least 16 MiB and a time budget
  for startup, manifest reads, and each invocation. A hook or runnable request
  can shorten that budget. The VM interrupt stops runaway Luau code;
  exhaustion returns a named `LimitCause`.
- By default, the per-call context contains verdict helpers but no host
  capabilities. Register callbacks with `Capabilities::insert` to expose
  them as `ctx.<name>(argument)`. The verdict-helper names are reserved, so
  a callback cannot shadow one. Each callback exchanges JSON-compatible
  values, and a returned error carries the capability name and reason.
  Capabilities run in the host: the VM cannot interrupt a blocking Rust
  callback or cap its memory. The host must bound that work itself.
