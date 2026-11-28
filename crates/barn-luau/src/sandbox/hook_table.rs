//! A bounded table of hook handlers, run in descending priority order.
//!
//! The field mapping follows tack#122's `HookHandler` declaration, not the
//! stall manifest: `priority`, `budgetMs`, `onError` and `run` become the
//! fields of [`HookHandler`]. A [`HookTable`] keeps equal priorities in their
//! input order, and [`run_table`] times each handler against a slice of the
//! remaining call budget. A thrown call follows its [`OnError`] policy, while
//! a hard sandbox limit denies the whole call with a named reason.

use std::cmp::Reverse;
use std::fmt;

use mlua::{Function, Value as LuaValue};

use crate::api::{Invocation, Outcome};

use super::Sandbox;
use super::errors::{EngineError, LimitCause, MAX_HANDLERS};
use super::outcome::decode_hook_result;
use super::values::json_to_lua;

pub use super::outcome::HookResult;

/// The hook name a table of before-handlers is installed under.
pub const EVENT_BEFORE: &str = "eventBefore";

const THROW_REASON: &str = "hook handler threw";
const TIMEOUT_REASON: &str = "hook handler budget exceeded";
const DEADLINE_REASON: &str = "sandbox deadline exceeded";
const MEMORY_REASON: &str = "sandbox memory limit";
const POISON_REASON: &str = "sandbox poisoned";
const TOO_DEEP_REASON: &str = "value too deep";
const TOO_LARGE_REASON: &str = "value too large";
const TOO_MANY_HANDLERS_REASON: &str = "too many hook handlers";
const TOO_MANY_ENTRIES_REASON: &str = "too many table entries";

/// What a handler does when its own call fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Skip the failing handler and let the table continue.
    Deliver,
    /// Stop the table and deny with the named failure reason.
    Deny,
}

/// A handler's Lua function, kept behind an opaque wrapper.
#[derive(Clone)]
pub struct HookRun {
    function: Function,
}

impl HookRun {
    #[must_use]
    pub const fn new(function: Function) -> Self {
        Self { function }
    }
}

impl fmt::Debug for HookRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookRun").finish_non_exhaustive()
    }
}

/// One entry in a hook table, mapped from tack#122's `HookHandler`.
pub struct HookHandler {
    pub priority: i32,
    pub budget_ms: u64,
    pub on_error: OnError,
    pub run: HookRun,
}

impl fmt::Debug for HookHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookHandler")
            .field("priority", &self.priority)
            .field("budget_ms", &self.budget_ms)
            .field("on_error", &self.on_error)
            .field("run", &self.run)
            .finish()
    }
}

/// A bounded, priority-ordered set of hook handlers.
#[derive(Debug)]
pub struct HookTable {
    handlers: Vec<HookHandler>,
}

impl HookTable {
    /// Sort handlers from highest to lowest priority, keeping equal-priority
    /// handlers in their input order.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Limit`] when there are more than
    /// [`MAX_HANDLERS`] handlers.
    pub fn new(mut handlers: Vec<HookHandler>) -> Result<Self, EngineError> {
        if handlers.len() > MAX_HANDLERS {
            return Err(EngineError::Limit(LimitCause::TooManyHandlers {
                limit: MAX_HANDLERS,
            }));
        }

        handlers.sort_by_key(|handler| Reverse(handler.priority));
        Ok(Self { handlers })
    }
}

/// What the table does after one handler: move on, or stop with a verdict.
///
/// Keeping "continue" as its own step avoids inventing an `Outcome` that
/// would be mistaken for a handler's allow verdict.
enum Step {
    Continue,
    Stop(Outcome),
}

/// Call one handler and decode its verdict.
fn call_handler(
    sandbox: &Sandbox,
    handler: &HookHandler,
    invocation: &Invocation,
) -> Result<HookResult, EngineError> {
    let payload = json_to_lua(&sandbox.lua, &invocation.payload, 0)?;
    let ctx = sandbox.ctx(&invocation.context)?;
    let value = handler.run.function.call::<LuaValue>((payload, ctx))?;
    decode_hook_result(value)
}

/// The named reason a hard sandbox limit denies with.
const fn stop_reason(cause: &LimitCause) -> &'static str {
    match cause {
        LimitCause::Deadline => DEADLINE_REASON,
        LimitCause::Memory => MEMORY_REASON,
        LimitCause::TooManyEntries { .. } => TOO_MANY_ENTRIES_REASON,
        LimitCause::TooManyHandlers { .. } => TOO_MANY_HANDLERS_REASON,
        LimitCause::TooLarge { .. } => TOO_LARGE_REASON,
        LimitCause::TooDeep { .. } => TOO_DEEP_REASON,
    }
}

/// Turn one handler's result into the table's next step.
///
/// An expired handler follows its [`OnError`] policy unless the shared call
/// cap was the binding budget: that limit is hard. A hard sandbox limit is
/// never softened by `on_error`.
fn classify(
    verdict: Result<HookResult, EngineError>,
    expired: bool,
    bound_by_call: bool,
    on_error: OnError,
) -> Step {
    if expired {
        return if bound_by_call {
            Step::Stop(Outcome::Deny {
                reason: DEADLINE_REASON.into(),
            })
        } else {
            match on_error {
                OnError::Deliver => Step::Continue,
                OnError::Deny => Step::Stop(Outcome::Deny {
                    reason: TIMEOUT_REASON.into(),
                }),
            }
        };
    }

    match verdict {
        Ok(HookResult::Continue) => Step::Continue,
        Ok(HookResult::Deny { reason }) => Step::Stop(Outcome::Deny { reason }),
        Ok(HookResult::Rewrite { payload }) => Step::Stop(Outcome::Rewrite { payload }),
        Ok(HookResult::Annotate { fields }) => Step::Stop(Outcome::Annotate { fields }),
        Ok(HookResult::Drop) => Step::Stop(Outcome::Drop),
        Err(EngineError::Limit(cause)) => Step::Stop(Outcome::Deny {
            reason: stop_reason(&cause).into(),
        }),
        Err(_) => match on_error {
            OnError::Deliver => Step::Continue,
            OnError::Deny => Step::Stop(Outcome::Deny {
                reason: THROW_REASON.into(),
            }),
        },
    }
}

/// Run a table against an invocation, stopping at the first terminal verdict.
///
/// Each handler gets the smaller of its own budget and the time left in the
/// call. A throwing handler follows its [`OnError`] policy; a hard sandbox
/// limit or an exhausted call cap denies the whole call with a named reason so
/// later handlers never run. The deadline guard restores any budget the
/// caller had armed on every return path.
#[must_use]
pub fn run_table(
    sandbox: &Sandbox,
    table: &HookTable,
    invocation: &Invocation,
    call_cap_ms: u64,
) -> Outcome {
    let Ok(_call_guard) = sandbox.lock_call() else {
        return Outcome::Deny {
            reason: POISON_REASON.into(),
        };
    };
    let _deadline_guard = sandbox.deadline.guard();
    let call_budget_us = u128::from(call_cap_ms).saturating_mul(1000);
    let call_start = sandbox.deadline.micros();

    for handler in &table.handlers {
        let elapsed_us = sandbox.deadline.micros().saturating_sub(call_start);
        let remaining_us = call_budget_us.saturating_sub(elapsed_us);
        if remaining_us == 0 {
            return Outcome::Deny {
                reason: DEADLINE_REASON.into(),
            };
        }

        let own_us = u128::from(handler.budget_ms).saturating_mul(1000);
        let bound_by_call = remaining_us <= own_us;
        sandbox.deadline.begin_us(own_us.min(remaining_us));
        let verdict = call_handler(sandbox, handler, invocation);
        let expired = sandbox.deadline.expired();
        sandbox.deadline.disarm();

        match classify(verdict, expired, bound_by_call, handler.on_error) {
            Step::Continue => {}
            Step::Stop(outcome) => return outcome,
        }
    }

    Outcome::Allow
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    use serde_json::{Map, Value, json};

    use super::super::tests::Scratch;
    use super::super::{Capabilities, Limits};
    use super::*;
    use crate::sandbox::clock::{Clock, FakeClock, TickingClock};

    const MEMORY: usize = 16 * 1024 * 1024;
    const DEADLINE_MS: u64 = 1000;

    fn limits() -> Limits {
        Limits::new(MEMORY, DEADLINE_MS).expect("valid limits")
    }

    const PLUGIN: &str = r#"
    return {
        name = "hooks",
        first = function(payload, ctx) ctx.note("first") return nil end,
        second = function(payload, ctx) ctx.note("second") return nil end,
        third = function(payload, ctx) ctx.note("third") return nil end,
        allow = function(payload, ctx) return ctx.allow() end,
        deny = function(payload, ctx) return ctx.deny("blocked") end,
        rewrite = function(payload, ctx) return ctx.rewrite({ command = "echo" }) end,
        annotate = function(payload, ctx) return ctx.annotate({ risk = "high" }) end,
        drop = function(payload, ctx) return ctx.drop() end,
        cont = function(payload, ctx) return nil end,
        throw = function(payload, ctx) error("boom") end,
        spin = function(payload, ctx) while true do end end,
        hog = function(payload, ctx)
            local items = {}
            while true do
                items[#items + 1] = string.rep("x", 4096) .. #items
            end
        end,
        spend_4999 = function(payload, ctx) ctx.advance(4999) return ctx.deny("just under") end,
        spend_5000 = function(payload, ctx) ctx.advance(5000) return nil end,
        spend_6000 = function(payload, ctx) ctx.advance(6000) return nil end,
        spend_11000 = function(payload, ctx) ctx.advance(11000) return nil end,
        backwards = function(payload, ctx) ctx.rewind() return nil end,
    }
    "#;

    /// Record `note` labels and let a handler spend fake-clock microseconds.
    fn capabilities(fake: Arc<FakeClock>) -> (Capabilities, Arc<Mutex<Vec<String>>>) {
        let order = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&order);
        let step = Arc::clone(&fake);
        let mut caps = Capabilities::default();
        caps.insert("note", move |value: Value| -> Result<Value, String> {
            let label = value.as_str().unwrap_or_default().to_owned();
            seen.lock().expect("order lock").push(label);
            Ok(Value::Null)
        });
        caps.insert("advance", move |value: Value| -> Result<Value, String> {
            let micros = value.as_u64().unwrap_or_default();
            step.advance(Duration::from_micros(micros));
            Ok(Value::Null)
        });
        (caps, order)
    }

    fn fixture(clock: Arc<dyn Clock>, caps: Capabilities) -> (Scratch, Sandbox) {
        let scratch = Scratch::new();
        scratch.write("plugin.luau", PLUGIN);
        let sandbox = Sandbox::new(scratch.path(), limits(), caps, clock).expect("fixture loads");
        (scratch, sandbox)
    }

    /// A static clock fixture so a test controls every elapsed microsecond.
    fn fake_fixture() -> (Arc<FakeClock>, Arc<Mutex<Vec<String>>>, Scratch, Sandbox) {
        let fake = Arc::new(FakeClock::new());
        let (caps, order) = capabilities(Arc::clone(&fake));
        let clock: Arc<dyn Clock> = Arc::clone(&fake);
        let (scratch, sandbox) = fixture(clock, caps);
        (fake, order, scratch, sandbox)
    }

    /// Each `micros` read advances the clock, so a loop reaches a checkpoint.
    fn tick_fixture() -> (Scratch, Sandbox) {
        let clock: Arc<dyn Clock> = Arc::new(TickingClock::new());
        fixture(clock, capabilities(Arc::new(FakeClock::new())).0)
    }

    fn handler(
        sandbox: &Sandbox,
        name: &str,
        priority: i32,
        budget_ms: u64,
        on_error: OnError,
    ) -> HookHandler {
        let function: Function = sandbox.plugin.get(name).expect("handler function");
        HookHandler {
            priority,
            budget_ms,
            on_error,
            run: HookRun::new(function),
        }
    }

    fn call() -> Invocation {
        Invocation {
            hook: EVENT_BEFORE.into(),
            payload: json!({ "command": "echo" }),
            context: Value::Null,
        }
    }

    #[test]
    fn runs_handlers_in_stable_descending_priority() {
        let (_fake, order, _scratch, sandbox) = fake_fixture();
        let table = HookTable::new(vec![
            handler(&sandbox, "third", 1, 100, OnError::Deny),
            handler(&sandbox, "first", 5, 100, OnError::Deny),
            handler(&sandbox, "second", 5, 100, OnError::Deny),
        ])
        .expect("table");

        assert_eq!(run_table(&sandbox, &table, &call(), 1000), Outcome::Allow);
        assert_eq!(
            *order.lock().expect("order"),
            vec!["first".to_owned(), "second".to_owned(), "third".to_owned()]
        );
    }

    #[test]
    fn maps_each_verdict_to_an_outcome() {
        let (_fake, _order, _scratch, sandbox) = fake_fixture();
        let mut fields = Map::new();
        fields.insert("risk".into(), json!("high"));

        for (name, expected) in [
            ("allow", Outcome::Allow),
            ("cont", Outcome::Allow),
            (
                "deny",
                Outcome::Deny {
                    reason: "blocked".into(),
                },
            ),
            (
                "rewrite",
                Outcome::Rewrite {
                    payload: json!({ "command": "echo" }),
                },
            ),
            ("annotate", Outcome::Annotate { fields }),
            ("drop", Outcome::Drop),
        ] {
            let table = HookTable::new(vec![handler(&sandbox, name, 0, 100, OnError::Deny)])
                .expect("table");
            assert_eq!(
                run_table(&sandbox, &table, &call(), 1000),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn a_real_infinite_loop_stops_on_the_budget() {
        let (_scratch, sandbox) = tick_fixture();

        for (on_error, expected) in [
            (OnError::Deliver, Outcome::Allow),
            (
                OnError::Deny,
                Outcome::Deny {
                    reason: TIMEOUT_REASON.into(),
                },
            ),
        ] {
            let table =
                HookTable::new(vec![handler(&sandbox, "spin", 0, 1, on_error)]).expect("table");
            assert_eq!(run_table(&sandbox, &table, &call(), 1000), expected);
        }
    }

    #[test]
    fn a_throw_follows_on_error() {
        let (_fake, _order, _scratch, sandbox) = fake_fixture();

        for (on_error, expected) in [
            (OnError::Deliver, Outcome::Allow),
            (
                OnError::Deny,
                Outcome::Deny {
                    reason: THROW_REASON.into(),
                },
            ),
        ] {
            let table =
                HookTable::new(vec![handler(&sandbox, "throw", 0, 100, on_error)]).expect("table");
            assert_eq!(run_table(&sandbox, &table, &call(), 1000), expected);
        }
    }

    #[test]
    fn accepts_exactly_the_handler_bound() {
        let (_fake, _order, _scratch, sandbox) = fake_fixture();
        let handlers = (0..MAX_HANDLERS)
            .map(|_| handler(&sandbox, "cont", 0, 100, OnError::Deny))
            .collect();

        let table = HookTable::new(handlers).expect("at the bound");
        assert_eq!(table.handlers.len(), MAX_HANDLERS);
    }

    #[test]
    fn refuses_more_than_the_handler_bound() {
        let (_fake, _order, _scratch, sandbox) = fake_fixture();
        let handlers = (0..=MAX_HANDLERS)
            .map(|_| handler(&sandbox, "cont", 0, 100, OnError::Deny))
            .collect();

        assert_eq!(
            HookTable::new(handlers).expect_err("over the bound"),
            EngineError::Limit(LimitCause::TooManyHandlers {
                limit: MAX_HANDLERS
            })
        );
    }

    #[test]
    fn exactly_budget_spent_is_expired() {
        let (_fake, _order, _scratch, sandbox) = fake_fixture();

        for (on_error, expected) in [
            (
                OnError::Deny,
                Outcome::Deny {
                    reason: TIMEOUT_REASON.into(),
                },
            ),
            (OnError::Deliver, Outcome::Allow),
        ] {
            let table = HookTable::new(vec![handler(&sandbox, "spend_5000", 0, 5, on_error)])
                .expect("table");
            assert_eq!(run_table(&sandbox, &table, &call(), 1000), expected);
        }
    }

    #[test]
    fn an_equal_handler_budget_and_call_cap_is_a_deadline_deny() {
        let (_fake, _order, _scratch, sandbox) = fake_fixture();

        for on_error in [OnError::Deny, OnError::Deliver] {
            let table = HookTable::new(vec![handler(&sandbox, "spend_5000", 0, 5, on_error)])
                .expect("table");
            assert_eq!(
                run_table(&sandbox, &table, &call(), 5),
                Outcome::Deny {
                    reason: DEADLINE_REASON.into(),
                },
                "{on_error:?}"
            );
        }
    }

    #[test]
    fn just_under_budget_keeps_the_verdict() {
        let (_fake, _order, _scratch, sandbox) = fake_fixture();
        let table = HookTable::new(vec![handler(&sandbox, "spend_4999", 0, 5, OnError::Deny)])
            .expect("table");

        assert_eq!(
            run_table(&sandbox, &table, &call(), 1000),
            Outcome::Deny {
                reason: "just under".into()
            }
        );
    }

    #[derive(Debug)]
    struct BackwardsClock {
        value: AtomicU64,
        rewound: AtomicBool,
    }

    impl BackwardsClock {
        fn new() -> Self {
            Self {
                value: AtomicU64::new(10_000),
                rewound: AtomicBool::new(false),
            }
        }

        fn rewind(&self) {
            self.rewound.store(true, Ordering::Relaxed);
        }
    }

    impl Clock for BackwardsClock {
        fn micros(&self) -> u128 {
            if self.rewound.load(Ordering::Relaxed) {
                u128::from(self.value.fetch_sub(1, Ordering::Relaxed))
            } else {
                u128::from(self.value.load(Ordering::Relaxed))
            }
        }
    }

    #[test]
    fn a_backwards_clock_expires() {
        let clock = Arc::new(BackwardsClock::new());
        let step = Arc::clone(&clock);
        let mut caps = Capabilities::default();
        caps.insert("rewind", move |_value: Value| -> Result<Value, String> {
            step.rewind();
            Ok(Value::Null)
        });
        let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock);
        let (_scratch, sandbox) = fixture(dyn_clock, caps);
        let table = HookTable::new(vec![handler(&sandbox, "backwards", 0, 100, OnError::Deny)])
            .expect("table");

        assert_eq!(
            run_table(&sandbox, &table, &call(), 1000),
            Outcome::Deny {
                reason: TIMEOUT_REASON.into()
            }
        );
    }

    #[test]
    fn a_deliver_handler_that_consumes_the_call_cap_stops_the_table() {
        let (_fake, order, _scratch, sandbox) = fake_fixture();
        let table = HookTable::new(vec![
            handler(&sandbox, "spend_5000", 0, 100, OnError::Deliver),
            handler(&sandbox, "second", -1, 100, OnError::Deny),
        ])
        .expect("table");

        assert_eq!(
            run_table(&sandbox, &table, &call(), 5),
            Outcome::Deny {
                reason: DEADLINE_REASON.into()
            }
        );
        assert_eq!(
            *order.lock().expect("order"),
            Vec::<String>::new(),
            "the second handler never runs"
        );
    }

    #[test]
    fn elapsed_call_budget_binds_the_later_handler() {
        let (_fake, order, _scratch, sandbox) = fake_fixture();
        let table = HookTable::new(vec![
            handler(&sandbox, "spend_6000", 2, 7, OnError::Deliver),
            handler(&sandbox, "spend_5000", 1, 100, OnError::Deliver),
            handler(&sandbox, "second", 0, 100, OnError::Deliver),
        ])
        .expect("table");

        assert_eq!(
            run_table(&sandbox, &table, &call(), 10),
            Outcome::Deny {
                reason: DEADLINE_REASON.into(),
            }
        );
        assert_eq!(
            *order.lock().expect("order"),
            Vec::<String>::new(),
            "the later handlers never run"
        );
    }

    #[test]
    fn a_call_cap_overrun_stops_before_the_next_handler() {
        let (_fake, order, _scratch, sandbox) = fake_fixture();
        let table = HookTable::new(vec![
            handler(&sandbox, "spend_11000", 1, 50, OnError::Deliver),
            handler(&sandbox, "second", 0, 100, OnError::Deny),
        ])
        .expect("table");

        assert_eq!(
            run_table(&sandbox, &table, &call(), 10),
            Outcome::Deny {
                reason: DEADLINE_REASON.into(),
            }
        );
        assert_eq!(
            *order.lock().expect("order"),
            Vec::<String>::new(),
            "the next handler never runs"
        );
    }

    #[test]
    fn a_deliver_failure_lets_a_later_handler_decide() {
        let (_fake, order, _scratch, sandbox) = fake_fixture();
        let table = HookTable::new(vec![
            handler(&sandbox, "throw", 0, 100, OnError::Deliver),
            handler(&sandbox, "second", -1, 100, OnError::Deny),
        ])
        .expect("table");

        assert_eq!(run_table(&sandbox, &table, &call(), 1000), Outcome::Allow);
        assert_eq!(
            *order.lock().expect("order"),
            vec!["second".to_owned()],
            "the later handler ran"
        );

        let deciding = HookTable::new(vec![
            handler(&sandbox, "throw", 0, 100, OnError::Deliver),
            handler(&sandbox, "deny", -1, 100, OnError::Deny),
        ])
        .expect("table");
        assert_eq!(
            run_table(&sandbox, &deciding, &call(), 1000),
            Outcome::Deny {
                reason: "blocked".into()
            }
        );
    }

    #[test]
    fn a_hard_limit_stops_the_table() {
        let (_fake, order, _scratch, sandbox) = fake_fixture();
        let table = HookTable::new(vec![
            handler(&sandbox, "hog", 0, 1000, OnError::Deliver),
            handler(&sandbox, "second", -1, 100, OnError::Deny),
        ])
        .expect("table");

        assert_eq!(
            run_table(&sandbox, &table, &call(), 1000),
            Outcome::Deny {
                reason: MEMORY_REASON.into()
            }
        );
        assert_eq!(
            *order.lock().expect("order"),
            Vec::<String>::new(),
            "a hard limit ignores on_error and the later handler never runs"
        );
    }

    #[test]
    fn the_guard_restores_an_outer_deadline() {
        let (fake, _order, _scratch, sandbox) = fake_fixture();
        sandbox.deadline.begin_us(5_000);
        assert!(!sandbox.deadline.expired());

        let table =
            HookTable::new(vec![handler(&sandbox, "cont", 0, 100, OnError::Deny)]).expect("table");
        assert_eq!(run_table(&sandbox, &table, &call(), 1000), Outcome::Allow);
        assert!(!sandbox.deadline.expired(), "outer budget restored");

        fake.advance(Duration::from_micros(5_000));
        assert!(sandbox.deadline.expired(), "outer budget still armed");
    }

    #[test]
    fn event_before_has_the_exact_hook_name() {
        assert_eq!(EVENT_BEFORE, "eventBefore");
    }
}
