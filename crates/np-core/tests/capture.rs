//! A `tracing` subscriber that keeps events in memory, for cases that assert on what np-core *emitted*.
//!
//! np-core installs no subscriber of its own — choosing one is np-cli's job (design §1) — so a case about an event
//! emitted inside this crate has to install its own. That subscriber is written here rather than borrowed from
//! `tracing-subscriber` on purpose: the design keeps that dependency out of every np-core build, and a dev-dependency
//! would still mean this crate's test tree compiles against a subscriber implementation it does not own. The whole
//! thing needed is "record the target, level and fields", which is forty lines of [`Subscriber`].
//!
//! It lives at `tests/capture.rs` rather than inside `tests/common/` for one reason: `mod common;` compiles that whole
//! module into every test binary of the crate, and only `tests/diagnostics.rs` wants this. Under
//! `cargo clippy --all-targets -D warnings` a shared file used by one binary is dead code in all the others — so this
//! file is `mod capture;`'d by the one binary that reads it, and neither side needs an allowance.
//!
//! # Why one subscriber and a gate
//!
//! `tracing::subscriber::set_global_default` can be called once per process, and cargo runs the cases in a binary on
//! several threads. So the subscriber is installed once, and [`case`] holds a mutex around each case: clear, run,
//! drain. Events from another case's thread cannot land inside a drained list, because that thread is blocked on the
//! same gate.

use std::sync::{Arc, Mutex, OnceLock};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};

/// One emitted event, as its emitter meant it: target, level, and every field as a string.
#[derive(Clone, Debug)]
pub struct Record {
    /// The static target string, e.g. `np_core::embed`.
    pub target: &'static str,
    /// How loud the emitter called it.
    pub level: Level,
    /// The event's message, if it has one.
    pub msg: Option<String>,
    /// Every other field, in emission order, each rendered with its `Display`.
    pub fields: Vec<(String, String)>,
}

impl Record {
    /// One field by name, e.g. the `rule` token this event reports.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Whether this event reports `rule`, the token a reader acts on.
    pub fn is_rule(&self, rule: &str) -> bool {
        self.field("rule") == Some(rule)
    }
}

/// The process-wide collector. Cheap to clone: one shared list behind the [`case`] gate.
#[derive(Default)]
pub struct Capture {
    events: Mutex<Vec<Record>>,
}

impl Subscriber for Capture {
    /// Record everything. A case that wants less would filter by target itself, and a subscriber that dropped events
    /// on the floor would make "nothing else was emitted" unfalsifiable.
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        // Spans are not part of the published surface (§1: one event per refusal), and this subscriber never opens one.
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = Collect::default();
        event.record(&mut visitor);
        let mut held = self
            .events
            .lock()
            .expect("diagnostics capture: no case panics while holding it");
        held.push(Record {
            target: event.metadata().target(),
            level: *event.metadata().level(),
            msg: visitor.msg,
            fields: visitor.fields,
        });
    }

    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Fields of one event: `message` is pulled out as [`Collect::msg`], everything else keeps its name.
#[derive(Default)]
struct Collect {
    msg: Option<String>,
    fields: Vec<(String, String)>,
}

impl Visit for Collect {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        // `tracing` renders a `%`-recorded Display field through `Debug`, which would leave quotes around every string.
        // The published values are paths, ids and tokens; unquoting keeps `field("rule") == Some("offline")` true rather
        // than making every case strip characters that are not part of the value.
        let trimmed = rendered
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .map(|inner| inner.replace("\\\"", "\""))
            .unwrap_or(rendered);
        if field.name() == "message" {
            self.msg = Some(trimmed);
        } else {
            self.fields.push((field.name().to_string(), trimmed));
        }
    }
}

/// One case at a time, because [`case`] clears the shared list. A `pub` item of a test file is invisible outside it, so
/// this is private to the binary that includes this module — which is the whole point of not putting it in `common`.
static GATE: Mutex<()> = Mutex::new(());

/// The one collector this process reports to, installed on first use.
fn subscriber() -> Arc<Capture> {
    static CAPTURE: OnceLock<Arc<Capture>> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let capture = Arc::new(Capture::default());
            tracing::subscriber::set_global_default(Arc::clone(&capture))
                .expect("test setup: no other subscriber is installed in this binary");
            capture
        })
        .clone()
}

/// Run `run` alone and return its value with every event it emitted, oldest first.
///
/// The gate is what makes "every event" mean "this case's events": a second case cannot emit while this one holds it.
pub fn case<T>(run: impl FnOnce() -> T) -> (T, Vec<Record>) {
    let capture = subscriber();
    // Poisoned-lock tolerance: a case that panics while emitting must not turn every later case into a cascade of
    // setup failures, which would hide the one real failure in noise.
    let _gate = GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    {
        let mut held = capture
            .events
            .lock()
            .expect("diagnostics capture: no case panics while holding it");
        held.clear();
    }
    let value = run();
    let events = capture
        .events
        .lock()
        .expect("diagnostics capture: no case panics while holding it")
        .clone();
    (value, events)
}
