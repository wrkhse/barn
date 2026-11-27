//! The bridge from barn's framed contract to the Luau sandbox.

use std::sync::Arc;
use std::time::Instant;

use barn_luau::api::{Invocation, Outcome as EngineOutcome};
use barn_luau::{Clock, EngineError, Sandbox};
use barn_stall::{Content, FRAMED_VERSION, Reply, Request};
use serde_json::{Value, json};

/// The machine name a frame with the wrong contract version gets.
const VERSION_ERROR: &str = "version_mismatch";

/// The name given to an engine variant this bridge does not know yet.
const UNKNOWN_ENGINE_ERROR: &str = "unknown_engine_error";

/// The verdict given to an engine outcome this bridge does not know yet.
const UNKNOWN_ENGINE_OUTCOME: &str = "unknown engine outcome";

#[derive(Debug)]
struct MonotonicClock {
    origin: Instant,
}

impl Clock for MonotonicClock {
    fn micros(&self) -> u128 {
        self.origin.elapsed().as_micros()
    }
}

/// A monotonic microsecond clock for the sandbox's call deadlines.
#[must_use]
pub fn real_clock() -> Arc<dyn Clock> {
    Arc::new(MonotonicClock {
        origin: Instant::now(),
    })
}

/// Answer one framed request with the verdict or result from `sandbox`.
pub fn handle_request(sandbox: &Sandbox, request: Request) -> Reply {
    match request {
        Request::Hook {
            id,
            version,
            hook,
            call,
            ..
        } => {
            if let Some(reply) = version_reply(id, version) {
                return reply;
            }

            let invocation = Invocation {
                hook,
                payload: call.input,
                context: json!({ "tool": call.tool }),
            };

            // `Sandbox` applies its own `call_deadline_ms` bound: main's
            // `Limits` cap production at 500 ms, while tests inject a longer
            // budget through their own `Limits`.
            match sandbox.hook(&invocation, u64::MAX) {
                Ok(outcome) => Reply::Hook {
                    id,
                    outcome: wire_outcome(outcome),
                },
                Err(error) => error_reply(id, &error),
            }
        }
        Request::Tool { id, version, call } => {
            if let Some(reply) = version_reply(id, version) {
                return reply;
            }

            // `Sandbox` applies its own `call_deadline_ms` bound: main's
            // `Limits` cap production at 500 ms, while tests inject a longer
            // budget through their own `Limits`.
            match sandbox.run(&call.name, &call.input, u64::MAX) {
                Ok(value) => Reply::Tool {
                    id,
                    content: Content {
                        kind: "text".to_owned(),
                        text: match value {
                            Value::String(text) => text,
                            other => other.to_string(),
                        },
                    },
                },
                Err(error) => error_reply(id, &error),
            }
        }
    }
}

fn version_reply(id: u64, version: u32) -> Option<Reply> {
    (version != FRAMED_VERSION).then(|| Reply::Error {
        id,
        error: VERSION_ERROR.to_owned(),
        message: format!("expected framed version {FRAMED_VERSION}, got {version}"),
    })
}

fn error_reply(id: u64, error: &EngineError) -> Reply {
    Reply::Error {
        id,
        error: error_name(error).to_owned(),
        message: error.to_string(),
    }
}

const fn error_name(error: &EngineError) -> &'static str {
    match error {
        EngineError::Load(_) => "load",
        EngineError::Script(_) => "script",
        EngineError::Capability { .. } => "capability",
        EngineError::Limit(_) => "limit",
        EngineError::UnknownTool(_) => "unknown_tool",
        EngineError::InvalidResult(_) => "invalid_result",
        EngineError::Poisoned => "poisoned",
        _ => UNKNOWN_ENGINE_ERROR,
    }
}

fn wire_outcome(outcome: EngineOutcome) -> barn_stall::Outcome {
    match outcome {
        EngineOutcome::Allow => barn_stall::Outcome::Allow,
        EngineOutcome::Deny { reason } => barn_stall::Outcome::Deny { reason },
        EngineOutcome::Rewrite { payload } => barn_stall::Outcome::Rewrite { input: payload },
        EngineOutcome::Annotate { fields } => barn_stall::Outcome::Annotate { fields },
        EngineOutcome::Drop => barn_stall::Outcome::Drop,
        _ => barn_stall::Outcome::Deny {
            reason: UNKNOWN_ENGINE_OUTCOME.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use barn_luau::api::Outcome as EngineOutcome;
    use barn_luau::{Capabilities, EngineError, LimitCause, Limits, LoadCause, Sandbox};
    use barn_stall::{Content, FRAMED_VERSION, Outcome, Reply, Request, ToolCall};
    use serde_json::{Map, Value, json};

    use super::*;

    const DEFAULT_MEMORY_BYTES: usize = 16 * 1024 * 1024;
    const DEFAULT_DEADLINE_MS: u64 = 100;
    const PLUGIN: &str = "return { name = 'bridge-test', tools = {} }";
    const HOOK_PLUGIN: &str = "return { name = 'bridge-hooks', PreToolUse = function(payload, ctx) return ctx.deny(ctx.tool .. ':' .. payload.command) end, tools = {} }";
    const TOOL_PLUGIN: &str = "return { name = 'bridge-tools', tools = { run = { name = 'run', run = function(input, ctx) return 'hello ' .. input.who end } } }";
    const PANIC_HOOK_PLUGIN: &str = "return { name = 'bridge-hook-version', PreToolUse = function() error('the hook must not run') end, tools = {} }";

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "barn-luau-bridge-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).expect("create scratch folder");

            Self { root }
        }

        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.root.join(name), contents).expect("write plugin fixture");
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            tool: "shell".to_owned(),
            name: "run".to_owned(),
            input: json!({ "command": "echo hi" }),
            session_id: None,
        }
    }

    fn fields() -> Map<String, Value> {
        std::iter::once(("chauffeurTriaged".to_owned(), json!(true))).collect()
    }

    fn fixture_sandbox(scratch: &Scratch, plugin: &str) -> Sandbox {
        scratch.write("plugin.luau", plugin);
        let limits = Limits::new(DEFAULT_MEMORY_BYTES, DEFAULT_DEADLINE_MS).expect("valid limits");

        Sandbox::new(
            scratch.path(),
            limits,
            Capabilities::default(),
            real_clock(),
        )
        .expect("fixture loads")
    }

    #[test]
    fn allow_maps_to_allow() {
        assert_eq!(wire_outcome(EngineOutcome::Allow), Outcome::Allow);
    }

    #[test]
    fn deny_maps_its_reason_unchanged() {
        let deny = EngineOutcome::Deny {
            reason: "push blocked".to_owned(),
        };

        assert_eq!(
            wire_outcome(deny),
            Outcome::Deny {
                reason: "push blocked".to_owned()
            }
        );
    }

    #[test]
    fn rewrite_renames_payload_to_input() {
        let payload = json!({ "command": "git push --dry-run" });
        let rewrite = EngineOutcome::Rewrite {
            payload: payload.clone(),
        };

        assert_eq!(wire_outcome(rewrite), Outcome::Rewrite { input: payload });
    }

    #[test]
    fn annotate_keeps_its_fields() {
        let annotate = EngineOutcome::Annotate { fields: fields() };

        assert_eq!(
            wire_outcome(annotate),
            Outcome::Annotate { fields: fields() }
        );
    }

    #[test]
    fn drop_maps_to_drop() {
        assert_eq!(wire_outcome(EngineOutcome::Drop), Outcome::Drop);
    }

    #[test]
    fn a_load_error_has_its_own_name() {
        let error = EngineError::Load(LoadCause::Message("load fail".to_owned()));

        assert_eq!(error_name(&error), "load");
        assert_eq!(
            error_reply(1, &error),
            Reply::Error {
                id: 1,
                error: "load".to_owned(),
                message: "load fail".to_owned(),
            }
        );
    }

    #[test]
    fn a_script_error_has_its_own_name() {
        let error = EngineError::Script("script fail".to_owned());

        assert_eq!(error_name(&error), "script");
        assert_eq!(
            error_reply(2, &error),
            Reply::Error {
                id: 2,
                error: "script".to_owned(),
                message: "script fail".to_owned(),
            }
        );
    }

    #[test]
    fn a_capability_error_has_its_own_name() {
        let error = EngineError::Capability {
            name: "lookup".to_owned(),
            reason: "not found".to_owned(),
        };

        assert_eq!(error_name(&error), "capability");
        assert_eq!(
            error_reply(3, &error),
            Reply::Error {
                id: 3,
                error: "capability".to_owned(),
                message: "capability lookup: not found".to_owned(),
            }
        );
    }

    #[test]
    fn a_limit_error_has_its_own_name() {
        let error = EngineError::Limit(LimitCause::Deadline);

        assert_eq!(error_name(&error), "limit");
        assert_eq!(
            error_reply(4, &error),
            Reply::Error {
                id: 4,
                error: "limit".to_owned(),
                message: "script deadline exceeded".to_owned(),
            }
        );
    }

    #[test]
    fn an_unknown_tool_error_has_its_own_name() {
        let error = EngineError::UnknownTool("nope".to_owned());

        assert_eq!(error_name(&error), "unknown_tool");
        assert_eq!(
            error_reply(5, &error),
            Reply::Error {
                id: 5,
                error: "unknown_tool".to_owned(),
                message: "unknown tool: nope".to_owned(),
            }
        );
    }

    #[test]
    fn an_invalid_result_error_has_its_own_name() {
        let error = EngineError::InvalidResult("bad result".to_owned());

        assert_eq!(error_name(&error), "invalid_result");
        assert_eq!(
            error_reply(6, &error),
            Reply::Error {
                id: 6,
                error: "invalid_result".to_owned(),
                message: "bad result".to_owned(),
            }
        );
    }

    #[test]
    fn a_poisoned_error_has_its_own_name() {
        let error = EngineError::Poisoned;

        assert_eq!(error_name(&error), "poisoned");
        assert_eq!(
            error_reply(7, &error),
            Reply::Error {
                id: 7,
                error: "poisoned".to_owned(),
                message: "engine call lock poisoned".to_owned(),
            }
        );
    }

    #[test]
    fn a_wrong_version_is_answered_before_the_sandbox_runs() {
        let scratch = Scratch::new();
        scratch.write("plugin.luau", PLUGIN);
        let limits = Limits::new(DEFAULT_MEMORY_BYTES, DEFAULT_DEADLINE_MS).expect("valid limits");
        let sandbox = Sandbox::new(
            scratch.path(),
            limits,
            Capabilities::default(),
            real_clock(),
        )
        .expect("fixture loads");
        let request = Request::Tool {
            id: 8,
            version: FRAMED_VERSION.wrapping_add(1),
            call: tool_call(),
        };

        assert_eq!(
            handle_request(&sandbox, request),
            Reply::Error {
                id: 8,
                error: "version_mismatch".to_owned(),
                message: "expected framed version 1, got 2".to_owned(),
            }
        );
    }

    #[test]
    fn the_real_clock_advances_in_microseconds() {
        let clock = real_clock();
        let start = Instant::now();
        let earlier = clock.micros();

        while start.elapsed() < Duration::from_millis(5) {
            std::hint::spin_loop();
        }

        let later = clock.micros();
        let wall_micros = start.elapsed().as_micros();

        assert!(later > earlier, "a busy wait must advance the clock");
        // The wall bracket starts before the first clock read and ends after
        // the last, so scheduler preemption between the reads is counted on
        // both sides. The clock shares its source with Instant, so its
        // microsecond delta tracks the measured wall delta. A generous 4x
        // band tolerates jitter, but a zero, millisecond or nanosecond clock
        // reporting micros cannot land inside it.
        let clock_delta = later.saturating_sub(earlier);
        let lower = wall_micros / 4;
        let upper = wall_micros.saturating_mul(4);
        assert!(
            (lower..=upper).contains(&clock_delta),
            "a {wall_micros} us busy wait must advance the clock within {lower}..={upper} micros, got {clock_delta}"
        );
    }

    #[test]
    fn a_hook_request_maps_the_verdict_from_the_plugin() {
        let scratch = Scratch::new();
        let sandbox = fixture_sandbox(&scratch, HOOK_PLUGIN);
        let request = Request::Hook {
            id: 10,
            version: FRAMED_VERSION,
            hook: "PreToolUse".to_owned(),
            call: tool_call(),
            _meta: None,
        };

        assert_eq!(
            handle_request(&sandbox, request),
            Reply::Hook {
                id: 10,
                outcome: Outcome::Deny {
                    reason: "shell:echo hi".to_owned(),
                },
            }
        );
    }

    #[test]
    fn a_tool_request_maps_the_runnable_result_to_text_content() {
        let scratch = Scratch::new();
        let sandbox = fixture_sandbox(&scratch, TOOL_PLUGIN);
        let request = Request::Tool {
            id: 11,
            version: FRAMED_VERSION,
            call: ToolCall {
                tool: "shell".to_owned(),
                name: "run".to_owned(),
                input: json!({ "who": "world" }),
                session_id: None,
            },
        };

        assert_eq!(
            handle_request(&sandbox, request),
            Reply::Tool {
                id: 11,
                content: Content {
                    kind: "text".to_owned(),
                    text: "hello world".to_owned(),
                },
            }
        );
    }

    #[test]
    fn a_hook_with_a_wrong_version_is_answered_before_the_sandbox_runs() {
        let scratch = Scratch::new();
        let sandbox = fixture_sandbox(&scratch, PANIC_HOOK_PLUGIN);
        let request = Request::Hook {
            id: 12,
            version: FRAMED_VERSION.wrapping_add(1),
            hook: "PreToolUse".to_owned(),
            call: tool_call(),
            _meta: None,
        };

        assert_eq!(
            handle_request(&sandbox, request),
            Reply::Error {
                id: 12,
                error: "version_mismatch".to_owned(),
                message: "expected framed version 1, got 2".to_owned(),
            }
        );
    }

    #[test]
    fn unknown_engine_names_are_stable() {
        assert_eq!(UNKNOWN_ENGINE_ERROR, "unknown_engine_error");
        assert_eq!(UNKNOWN_ENGINE_OUTCOME, "unknown engine outcome");
    }
}
