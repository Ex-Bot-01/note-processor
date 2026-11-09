//! The diagnostic sinks: what `np` records about itself, and where it puts that record.
//!
//! **Why this lives in the binary and not in np-core.** A log sink is process-global state — one subscriber, one stderr, one
//! file handle — and only `np` owns a process. np-core is instrumented (the level vocabulary in [`np_core::config`], and from
//! here on the spans and events) and installs nothing, which is what lets someone embed it without their own `log` lines
//! turning up in their terminal, and what makes an unasked-for diagnostic cost a compare instead of a `format!`. This module is
//! the half that decides to *write*, and it is deliberately unreachable from np-core.
//!
//! # The three rules this file exists to keep
//!
//! * **stdout is protocol.** Nothing here writes to stdout. Ever. `np serve` has a test that refuses any stderr line starting
//!   with `{`, which is the same rule seen from the other side: JSON belongs in a file, human text belongs on stderr.
//! * **Silence is the default** (spec point 1). The capture level is `warn`; stderr prints diagnostics only when someone asked,
//!   by flag, environment or config file. Pinned user-facing sentences are output rather than logs: they are not routed through
//!   here, so they can be neither suppressed nor counted.
//! * **A diagnostic is never the reason a command fails.** An unopenable log path warns once and carries on; a full budget
//!   stops writing and says so in one final record; a deleted file costs records, not the run.
//!
//! Design: `docs/superpowers/specs/2026-12-10-np-diagnostics-design.md`.
//!
//! # Two phases, because configuration is itself a thing that can fail
//!
//! [`bootstrap`] runs before any config file is read, so the most common integrator failure — a `config.toml` with one section
//! name one letter off (**F35**) — already has somewhere to be reported. [`apply_config`] upgrades it once the file has loaded:
//! the level may change, and `[log] file` opens then. Both mutate the *same* sink through an `Arc`, because tracing's global
//! default can be set only once; a level chosen at start-up is therefore still replaceable mid-run without a reload handle or a
//! second subscriber.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use np_core::config::{Config, DiagSource, DiagSpec, Level, DEFAULT_LOG_MAX_BYTES, DIAG_ENV_VAR};
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{span, Event, Level as TracingLevel, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::{LookupSpan, SpanRef};

/// Whether writing a diagnostic to descriptor 2 **now** would block.
///
/// `poll` with a zero timeout answers without waiting: for a pipe or socket whose buffer is full it says "not now", which
/// is the client not reading — the exact situation that used to stall `np serve`. A terminal and a regular file always
/// answer ready, so nothing about ordinary `-v` runs changes.
///
/// One syscall per diagnostic line, taken only on a line the level filter already passed. The alternative that looks
/// cheaper — putting descriptor 2 into `O_NONBLOCK` for the duration of the write — is not ours to take: the flag belongs
/// to the open file description, shared with every other writer in the process, so it would put the *pinned* user-facing
/// sentences (the refusal lines, the announce line) at risk of vanishing into `EAGAIN`. Dropping our own line loses a
/// diagnostic and nothing else.
///
/// A failed `poll` answers "would not block": an error here means the descriptor is odd rather than slow, and the write
/// below already tolerates any error that descriptor can give.
#[cfg(unix)]
fn stderr_would_block() -> bool {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: io::stderr().as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: `fd` is one initialised `pollfd`, and the count passed matches it. `poll` writes only `revents`.
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    // Three answers, and only one of them means "write". `ready == 0` is the timeout — the pipe buffer is full and nobody
    // is reading; `ready > 0` without `POLLOUT` is the same news with an error condition attached (`POLLERR`/`POLLHUP` on
    // a closed or broken pipe). A negative return means `poll` itself failed (an odd descriptor, `EINTR`), and that one
    // answers "would not block": the write below already tolerates any error the descriptor can give.
    if ready < 0 {
        return false;
    }
    ready == 0 || (fd.revents & libc::POLLOUT) == 0
}

/// Windows has no `poll`; a write to a full pipe there blocks as it always did, and
/// `docs/reference-diagnostics.md` says so plainly rather than pretending otherwise. The cap still bounds how many lines
/// reach that pipe, which bounds how long the wait can be, and `[log] file` is the answer for a client that never reads
/// stderr at all.
#[cfg(not(unix))]
fn stderr_would_block() -> bool {
    false
}

/// How many diagnostic lines one process may print to stderr before it stops (spec point 8).
///
/// A normal `-v` run reaches tens of lines. The runs that blow past a thousand are trace-level loops over thousands of notes,
/// which is exactly when stderr is the wrong place and a file is the right one — so the cap is not a courtesy, it is the sign
/// post to `[log] file`.
const STDERR_CAP: usize = 1_000;

/// The published keys of a JSONL record, in the order `docs/reference-diagnostics.md` lists them.
///
/// **The file format is frozen** even though the human format is not, so this list is what a field must appear in before code
/// may emit it: [`Record::push`] asserts in a debug build that every field is one of these, and `crates/np-cli/tests/diag.rs`
/// asserts every key that reached a real log file is here too. A key added without being written down therefore fails a test
/// rather than surprising someone six months later who was parsing the file.
const RECORD_KEYS: [&str; 13] = [
    "ts",
    "level",
    "target",
    "msg",
    "request",
    "workspace",
    "rule",
    "what",
    "expected",
    "actual",
    "elapsed_ms",
    "bytes",
    "action",
];

/// The suppression notice, printed once at exit with the count it owes.
fn truncation_notice(suppressed: usize) -> String {
    format!(
        "diagnostics truncated after {STDERR_CAP} lines; {suppressed} more suppressed \
         (raise --log-spec or set [log] file)"
    )
}

/// The ordinal of a level, used only for comparisons inside this file: `off` is 0 and `trace` is 5, so "let this through?" is
/// one `<=`. This is not a published ordering — the spellings in [`np_core::config::DIAG_LEVEL_NAMES`] are the contract, and
/// nothing outside this file needs the numbers.
fn ordinal(level: Level) -> u8 {
    match level {
        Level::Off => 0,
        Level::Error => 1,
        Level::Warn => 2,
        Level::Info => 3,
        Level::Debug => 4,
        Level::Trace => 5,
    }
}

/// The same for a `tracing` level. `off` has no counterpart (a callsite is either enabled or not), and an unrecognised future
/// level sorts as the deepest one because reaching this function at all means something already enabled it.
fn tracing_ordinal(level: &TracingLevel) -> u8 {
    if *level == TracingLevel::ERROR {
        1
    } else if *level == TracingLevel::WARN {
        2
    } else if *level == TracingLevel::INFO {
        3
    } else if *level == TracingLevel::DEBUG {
        4
    } else {
        5
    }
}

/// The level's spelling in a record and on stderr: lowercase, matching the file format.
///
/// `tracing::Level::as_str` answers `ERROR`, which would put capitals in an immutable JSON schema and make the human line
/// disagree with the machine one about what `"level"` holds.
fn level_text(level: &TracingLevel) -> &'static str {
    if *level == TracingLevel::ERROR {
        "error"
    } else if *level == TracingLevel::WARN {
        "warn"
    } else if *level == TracingLevel::INFO {
        "info"
    } else if *level == TracingLevel::DEBUG {
        "debug"
    } else {
        "trace"
    }
}

/// One file sink's state: the handle, how much has gone through it, and whether it has said its last word.
struct FileSink {
    file: fs::File,
    /// Bytes already on this path when the sink opened, so a log that filled its budget in an earlier run stays full instead of
    /// gaining one more record per invocation.
    written: u64,
    max_bytes: u64,
    /// Set once the budget is spent: writing stops, and one `log full` record is the last line.
    full: bool,
}

impl FileSink {
    /// Open (or adopt) the log file in append mode, creating its parent directory the way the data dir is created.
    ///
    /// The existing size counts against the budget on purpose: `[log] max_bytes` bounds *the file*, not the run, and with no
    /// rotation (spec point 8 refuses it, because a half-finished rename is a worse story than a log that stops honestly)
    /// there is no other way to keep that promise across runs.
    fn open(path: &Path, max_bytes: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        // Already at or past the budget: nothing more will be written, and no second `log full` line is added. The remedy is
        // already in the file for whoever reads it, and one more record per run would grow a file whose whole point is to stop.
        let full = written >= max_bytes;
        Ok(Self {
            file,
            written,
            max_bytes,
            full,
        })
    }

    /// Append one record, then — if that write spent the budget — the honest stop record.
    fn write(&mut self, line: &str) {
        if self.full {
            return;
        }
        // **One `write_all` per whole record, newline included.** Two processes appending to one file is a documented shape (a
        // serve child plus a concurrent `np scan`), and the first version of this function wrote the line and the `\n` in two
        // calls — which let the other process's complete record land between them and produced one line that was neither
        // record. `concurrent_processes_never_split_a_record` is what caught it, and it is why the append is not split again.
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        if self.file.write_all(&bytes).is_err() {
            return;
        }
        self.written += bytes.len() as u64;
        if self.written < self.max_bytes {
            return;
        }
        self.full = true;
        let stop = serde_json::json!({
            "ts": now_ms(),
            "level": "warn",
            "target": "np_cli",
            "msg": "log full",
            "bytes": self.written,
            "action": "delete the file to continue",
        });
        if let Ok(text) = serde_json::to_string(&stop) {
            let mut bytes = Vec::with_capacity(text.len() + 1);
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(b'\n');
            let _ = self.file.write_all(&bytes);
        }
    }
}

/// Everything the sinks know, shared between the subscriber and the code that reconfigures it.
struct Sink {
    /// The highest level any target allows, as an ordinal — `0` when nothing would be written anywhere. One atomic read answers
    /// "no" for a disabled callsite without touching a lock, which is what "free when off" is physically made of.
    ceiling: AtomicU8,
    /// The default level's ordinal, for the announce line and the opening record.
    default_level: AtomicU8,
    /// Whether stderr prints diagnostics at all — someone has to have asked (spec point 1).
    stderr_on: AtomicBool,
    /// Set once a file sink is open, so an event with nowhere to go builds no JSON.
    has_file: AtomicBool,
    /// Diagnostics already printed to stderr, counted *before* formatting so a suppressed line costs a counter.
    printed: AtomicUsize,
    suppressed: AtomicUsize,
    /// The announce line prints at most once per process, from whichever phase first learns stderr is on.
    announced: AtomicBool,
    /// Per-target overrides as ordinals, in the order written.
    overrides: RwLock<Vec<(String, u8)>>,
    /// The file sink, or `None` when nobody configured one — a failed open warns once and leaves this `None`.
    file: Mutex<Option<FileSink>>,
    /// Set once `[log] file` has been attempted, so an unopenable path warns once rather than on every reconfigure.
    file_attempted: AtomicBool,
}

impl Sink {
    fn new() -> Self {
        Self {
            ceiling: AtomicU8::new(0),
            default_level: AtomicU8::new(ordinal(DiagSpec::default().default)),
            stderr_on: AtomicBool::new(false),
            has_file: AtomicBool::new(false),
            printed: AtomicUsize::new(0),
            suppressed: AtomicUsize::new(0),
            announced: AtomicBool::new(false),
            overrides: RwLock::new(Vec::new()),
            file: Mutex::new(None),
            file_attempted: AtomicBool::new(false),
        }
    }

    /// Whether a record at `level` on `target` is captured — the one question that decides whether anything gets formatted, so
    /// the common "off" case stays allocation-free and lock-free.
    fn allows(&self, target: &str, level: u8) -> bool {
        if level == 0 || level > self.ceiling.load(Ordering::Relaxed) {
            return false;
        }
        // The overrides list is short (a handful of targets, written once at start-up), and this only runs for a record some
        // target already allows.
        let overrides = self.overrides.read().expect("diagnostic overrides lock");
        match overrides.iter().find(|(name, _)| name == target) {
            Some((_, max)) => level <= *max,
            None => level <= self.default_level.load(Ordering::Relaxed),
        }
    }

    /// Take one stderr slot if the cap allows it, counting a refusal so the closing notice can name it.
    fn take_stderr_slot(&self) -> bool {
        let taken = self.printed.fetch_add(1, Ordering::Relaxed);
        if taken < STDERR_CAP {
            return true;
        }
        self.printed.fetch_sub(1, Ordering::Relaxed);
        self.suppressed.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Apply a resolved spec. `capturing` says whether anything would be written at all: with no stderr and no file, the
    /// ceiling drops to 0 so an unasked-for event costs one atomic read instead of formatting into a void.
    fn set_spec(&self, spec: &DiagSpec, capturing: bool) {
        let default = ordinal(spec.default);
        let overrides: Vec<(String, u8)> = spec
            .per_target
            .iter()
            .map(|(target, level)| (target.clone(), ordinal(*level)))
            .collect();
        // The ceiling is the widest thing any target allows; `off` on one target must not lower it for the others.
        let ceiling = if capturing {
            overrides
                .iter()
                .map(|(_, max)| *max)
                .chain(std::iter::once(default))
                .max()
                .unwrap_or(default)
        } else {
            0
        };
        {
            let mut slot = self.overrides.write().expect("diagnostic overrides lock");
            *slot = overrides;
        }
        self.ceiling.store(ceiling, Ordering::Relaxed);
        self.default_level.store(default, Ordering::Relaxed);
    }

    /// Write one line to stderr, through the cap.
    fn write_stderr(&self, line: &str) {
        if !self.take_stderr_slot() {
            return;
        }
        // **A diagnostic is never the reason a command waits.** This module's third rule used to be true only of failures
        // — an unopenable path, a full budget — and not of descriptor 2 itself: measured on this box, `np serve` with
        // `NP_LOG=error` against a config that refuses every request stalled inside a stderr write once the client's pipe
        // filled at ~60 KiB, so the requests behind it went unanswered. A log channel that can stop the thing it is
        // watching is not a log channel.
        //
        // So: ask first, drop when the answer is "not now" (see [`stderr_would_block`] for what the question costs and why
        // it is not `O_NONBLOCK`). A dropped line counts as suppressed exactly as one past the cap does, so the closing
        // notice still appears and names both ways out; the file sink is unaffected, because a regular file does not fill.
        if stderr_would_block() {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // Written through a handle with the result dropped, for the reason `np_core::scan` gives for its own stderr receipt:
        // `eprintln!` panics when descriptor 2 refuses the write, which would replace a reportable failure with exit 101 and a
        // panic message that cannot be printed either.
        let mut err = io::stderr();
        let _ = writeln!(err, "{line}");
    }

    /// Write one record to the file sink, if there is one.
    fn write_file(&self, record: &Value) {
        let Ok(text) = serde_json::to_string(record) else {
            return;
        };
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sink) = guard.as_mut() {
            sink.write(&text);
        }
    }
}

/// The process's one sink. A `OnceLock` rather than a passed-around value because tracing's global default can be set exactly
/// once, and the two phases in this module's header need to *change* what that subscriber does without replacing it.
static SINK: OnceLock<Arc<Sink>> = OnceLock::new();

/// The sink for this process, installing the subscriber on first call.
fn sink() -> Arc<Sink> {
    Arc::clone(SINK.get_or_init(|| {
        let shared = Arc::new(Sink::new());
        let subscriber = tracing_subscriber::registry::Registry::default().with(DiagLayer {
            sink: Arc::clone(&shared),
        });
        // `set_global_default` fails only if something already installed a subscriber. Nothing in this binary does — np-core
        // installs none by design — so a failure here would be a regression in this file, and the fallback is honest: keep
        // running with whatever sink is already there rather than panic over a log.
        let _ = tracing::subscriber::set_global_default(subscriber);
        shared
    }))
}

/// Handle on the installed sinks.
///
/// Returned by [`install`] so a caller holds a reference to what it configured; the state itself is process-global, because a
/// log sink that two parts of the program owned would be two logs.
pub(crate) struct DiagHandle {
    shared: Arc<Sink>,
}

impl DiagHandle {
    /// Print the announce line if stderr is on and nobody has printed it yet (spec point 2: the environment's silence is
    /// otherwise invisible, so `-v` says the effective level **and** where it came from).
    fn announce(&self, spec: &DiagSpec, source: DiagSource) {
        if !self.shared.stderr_on.load(Ordering::Relaxed)
            || self.shared.announced.swap(true, Ordering::Relaxed)
        {
            return;
        }
        // A plain sentence with no level prefix and no target: this is the run telling you what it will do, not a log line, so
        // it is neither counted against the cap nor suppressed by it.
        let mut err = io::stderr();
        let _ = writeln!(
            err,
            "log level {} from {}",
            spec.default.as_str(),
            source.as_str()
        );
    }
}

/// Install the sinks for this run: `spec` decides what is captured, `file` where records also go, `max_bytes` when that file
/// stops, and `source` which knob won — which is both the announce line's second half and the answer to "did anyone ask?".
///
/// Called twice per process, deliberately (see the module header): once from argv and the environment before any config file is
/// read, once after it loads. The second call mutates the same sink, so a `[log] level` that beats the environment still wins
/// over the default, and `[log] file` opens as soon as there is a file to name.
///
/// There is no separate `announce: bool`, though the plan sketched one: stderr prints diagnostics **iff** somebody asked, which
/// is exactly `source != DiagSource::Default`. Two knobs for one rule could only disagree with each other.
pub(crate) fn install(
    spec: &DiagSpec,
    file: Option<&Path>,
    max_bytes: u64,
    source: DiagSource,
) -> DiagHandle {
    let shared = sink();
    let asked = source != DiagSource::Default;
    shared.stderr_on.store(asked, Ordering::Relaxed);
    if let Some(path) = file {
        open_log(&shared, path, max_bytes, spec, source);
    }
    // Ordered after the open so `capturing` can see the file: at default level with no file and nobody asking, nothing is
    // written anywhere, and the ceiling goes to 0 rather than formatting warn-level events for a sink that does not exist.
    shared.set_spec(spec, asked || shared.has_file.load(Ordering::Relaxed));
    let handle = DiagHandle { shared };
    handle.announce(spec, source);
    handle
}

/// Open (or adopt) the log file and write the run's opening record.
fn open_log(shared: &Sink, path: &Path, max_bytes: u64, spec: &DiagSpec, source: DiagSource) {
    if shared.file_attempted.swap(true, Ordering::Relaxed) {
        // One attempt per process: a path that refused once is not going to succeed because we asked again, and the warning
        // must not repeat. A `--log-file` flag also outranks `[log] file`, so the first attempt is the one that should win.
        return;
    }
    match FileSink::open(path, max_bytes) {
        Ok(sink) => {
            // The first record of every run names the settings that produced the lines after it: a log found days later has
            // to say what was switched on, and by what. Written directly rather than as a `tracing` event, because it is the
            // one record that must exist even when the capture level would have filtered it out.
            let opening = serde_json::json!({
                "ts": now_ms(),
                "level": spec.default.as_str(),
                "target": "np_cli",
                "msg": format!("log level {} from {}", spec.default.as_str(), source.as_str()),
            });
            let text = serde_json::to_string(&opening).unwrap_or_default();
            let mut guard = shared
                .file
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(sink);
            if let Some(sink) = guard.as_mut() {
                sink.write(&text);
            }
            drop(guard);
            shared.has_file.store(true, Ordering::Relaxed);
        }
        Err(err) => {
            // Warn once and continue: diagnostics are never the reason a command fails, as with every other sink here
            // (`scan`'s catalog write, `db`'s durable record).
            let mut err_out = io::stderr();
            let _ = writeln!(
                err_out,
                "np: log file {} could not be opened ({err}); diagnostics will not be written there",
                path.display()
            );
        }
    }
}

/// Resolve the levels from a loaded config and upgrade the sinks — the second phase.
///
/// Idempotent by design: several commands read configuration more than once per run, and a reconfigure that reopened the log
/// file each time would produce one opening record per call.
pub(crate) fn apply_config(cfg: &Config) {
    static APPLIED: OnceLock<()> = OnceLock::new();
    if APPLIED.set(()).is_err() {
        return;
    }
    let (spec, source) =
        match np_core::config::resolve_diag(None, env_value().as_deref(), log_level(cfg)) {
            Ok(resolved) => resolved,
            // Reachable only through `NP_LOG`, and only when the value is not valid text: refusing here would make a diagnostics
            // typo fail a command, which is the one thing this feature promises never to do. So it warns, and keeps the levels
            // already in force from phase one — where the same value was refused or accepted by the same rule.
            Err(err) => {
                let mut err_out = io::stderr();
                let _ = writeln!(err_out, "np: {err}; keeping the log level already in force");
                return;
            }
        };
    install(
        &spec,
        cfg.log.file.as_deref(),
        cfg.log.max_bytes.unwrap_or(DEFAULT_LOG_MAX_BYTES),
        source,
    );
    // The announce line belongs *after* configuration when the file is what chose the level: that is the first moment the answer
    // is final. One ordering caveat is accepted rather than worked around — on Windows, **F37**'s legacy-data-dir note prints
    // inside `load_config`, so it can precede this line. It is a one-release notice, and both lines are stderr anyway.
}

/// `NP_LOG` read the way this program reads every environment value: absent or empty is silence (`env_text`'s rule), and text
/// that is not valid UTF-8 is a refusal naming the variable rather than a guess at it.
fn env_value() -> Option<String> {
    match std::env::var_os(DIAG_ENV_VAR) {
        None => None,
        Some(value) if value.is_empty() => None,
        Some(value) => value.into_string().ok(),
    }
}

/// `[log] level` as written in the file, if it was written.
fn log_level(cfg: &Config) -> Option<&str> {
    cfg.log.level.as_deref()
}

/// Compose the flag half of the spec: `-v`/`-vv`/`-vvv` set the level, `--log-spec` adds per-target overrides, and a bare level
/// written inside `--log-spec` wins over the count because it is the more specific spelling of the same thing.
pub(crate) fn flag_spec(verbose: u8, log_spec: Option<&str>) -> Result<Option<String>, String> {
    let counted = match verbose {
        0 => None,
        1 => Some(Level::Info),
        2 => Some(Level::Debug),
        _ => Some(Level::Trace),
    };
    let Some(raw) = log_spec else {
        return Ok(counted.map(|level| level.as_str().to_string()));
    };
    // Parse what was written so the two spellings merge by rule rather than by string surgery — and so a bad value is refused
    // here naming `--log-spec`, the same "name the variable" rule that governs `NP_LOG`.
    let parsed = np_core::config::parse_diag_spec(raw)
        .map_err(|err| format!("{}: {err}", np_core::config::DIAG_FLAG))?;
    // "Was a default written?" is a question about the text, not about the value: `--log-spec warn` must beat `-vv`, and
    // comparing against the default level would read that as silence.
    let bare_written = raw
        .split(',')
        .next()
        .is_some_and(|first| !first.trim().contains('='));
    let mut text = match (counted, bare_written) {
        (Some(level), false) => level.as_str().to_string(),
        (_, true) => parsed.default.as_str().to_string(),
        (None, false) => DiagSpec::default().default.as_str().to_string(),
    };
    for (target, level) in &parsed.per_target {
        text.push(',');
        text.push_str(target);
        text.push('=');
        text.push_str(level.as_str());
    }
    Ok(Some(text))
}

/// Bootstrap the sinks from argv, the environment and a lenient peek at `[log]`, before the config file is validated.
///
/// The `Err` text is a refusal to run: a bad `NP_LOG` or `--log-spec` is reported as every other argument-level failure is —
/// one sentence on stderr, exit 1. A run whose log level nobody can read has no business guessing at a quieter one, and the
/// whole point of naming the variable is that the reader can fix it in one edit.
pub(crate) fn bootstrap(
    verbose: u8,
    log_spec: Option<&str>,
    log_file: Option<&Path>,
) -> Result<DiagHandle, String> {
    let flag = flag_spec(verbose, log_spec)?;
    // The `[log]` table is **peeked** here — read leniently, ignoring a file that refuses for any other reason — because the
    // level announced on stderr has to be the answer that is *final*, and the file outranks the environment (§12 rule 7).
    // Without the peek, `NP_LOG=trace` next to `[log] level = "info"` would announce `trace from NP_LOG` and then quietly run
    // at info: precisely the invisibility the announce line exists to remove. The second benefit is the one spec point 3 asks
    // for — a config that refuses for an unrelated reason still says where its diagnostics go.
    let peek = np_core::config::peek_log_config();
    let (spec, source) = np_core::config::resolve_diag(
        flag.as_deref(),
        env_value().as_deref(),
        peek.level.as_deref(),
    )?;
    // The flag's path outranks the file's, exactly as the flag's level does; with neither, nothing is opened.
    let path = log_file.or(peek.file.as_deref());
    Ok(install(
        &spec,
        path,
        peek.max_bytes.unwrap_or(DEFAULT_LOG_MAX_BYTES),
        source,
    ))
}

/// Say what stderr swallowed, once, at the end: the cap counts during the run and the notice owes the total.
pub(crate) fn finish() {
    let Some(shared) = SINK.get() else {
        return;
    };
    let suppressed = shared.suppressed.load(Ordering::Relaxed);
    if suppressed == 0 {
        return;
    }
    let mut err = io::stderr();
    let _ = writeln!(err, "{}", truncation_notice(suppressed));
}

/// Unix milliseconds, as a JSON number. The file format's `ts` is this and nothing else (spec point 7).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// One event's fields, gathered once and rendered twice.
#[derive(Default)]
struct Record {
    msg: String,
    fields: Vec<(String, Value)>,
    /// A record whose sentence already reached stderr through a pinned user-facing path: it belongs in the file and must not
    /// print a second time there.
    already_printed: bool,
}

impl Record {
    fn push(&mut self, name: &str, value: Value) {
        // The published key set is a promise about an immutable format, so an undocumented field is a programming mistake —
        // caught in every debug run (which is what the gate list runs) rather than in a reader's parser six months out.
        debug_assert!(
            RECORD_KEYS.contains(&name),
            "`{name}` is not in np's published diagnostic keys: {RECORD_KEYS:?}"
        );
        self.fields.push((name.to_string(), value));
    }

    /// The human line for stderr: level, target, sentence, then the fields. Deliberately unfrozen — a machine parses the file.
    fn human(&self, level: &str, target: &str) -> String {
        let mut line = format!("{level} {target}: {}", self.msg);
        for (name, value) in &self.fields {
            match value {
                Value::String(text) => line.push_str(&format!(" {name}={text:?}")),
                other => line.push_str(&format!(" {name}={other}")),
            }
        }
        line
    }

    /// The JSONL record: `ts`, `level`, `target`, `msg`, then whatever fields the event carried.
    fn json(&self, level: &str, target: &str) -> Value {
        let mut object = Map::new();
        object.insert("ts".to_string(), Value::from(now_ms()));
        object.insert("level".to_string(), Value::from(level));
        object.insert("target".to_string(), Value::from(target));
        object.insert("msg".to_string(), Value::from(self.msg.as_str()));
        for (name, value) in &self.fields {
            object.insert(name.clone(), value.clone());
        }
        Value::Object(object)
    }
}

/// Collects field values without building anything that will be thrown away.
impl Visit for Record {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.msg = format!("{value:?}");
        } else {
            self.push(field.name(), Value::from(format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.msg = value.to_string();
        } else {
            self.push(field.name(), Value::from(value));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        // `user` is a marker for the sink rather than a published key: it says the sentence already reached stderr.
        if field.name() == "user" {
            self.already_printed = value;
            return;
        }
        self.push(field.name(), Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field.name(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field.name(), Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field.name(), Value::from(value));
    }
}

/// What a span hands to every record opened inside it (spec §"Serve": *each request span carries the JSON-RPC `id` (or
/// none) and the workspace path*).
///
/// Only two keys are inherited — `request` and `workspace` — because those are the two a reader cannot recover from the
/// line itself once several vaults and several requests share one process. Anything else a span carries belongs to the
/// span's own close record, not to every event nested in it.
///
/// **Absent means omitted.** A notification-shaped request has no id, and `serve_channel.rs` already refuses to write
/// `"id": null` on the wire for the same reason: a frontend that sees `null` cannot tell "no id was given" from "the id
/// is the value null", nor either from "this line belongs to no request". So there is no `request` entry at all, and
/// [`DiagLayer::on_event`] copies only entries that exist.
#[derive(Debug)]
struct SpanInfo {
    /// The inheritable keys this span declares.
    declared: Vec<(String, Value)>,
    /// When the phase began, so [`DiagLayer::on_close`] can report what it cost (spec point 7).
    started: Instant,
}

/// Whether a span field is one a nested record should carry.
fn inherited_key(name: &str) -> bool {
    name == "request" || name == "workspace"
}

/// The fields a span was opened with, keeping only the inheritable ones.
#[derive(Default)]
struct SpanFields {
    kept: Vec<(String, Value)>,
}

impl Visit for SpanFields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if inherited_key(field.name()) {
            // `%path.display()` arrives here as a display wrapper whose `Debug` is its `Display`, so this is the bare
            // path rather than a quoted copy of it — which is what makes `record["workspace"]` a string worth comparing
            // instead of a string with punctuation glued on.
            let rendered = format!("{value:?}");
            self.kept
                .push((field.name().to_string(), Value::from(rendered)));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if inherited_key(field.name()) {
            self.kept
                .push((field.name().to_string(), Value::from(value)));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if inherited_key(field.name()) {
            self.kept
                .push((field.name().to_string(), Value::from(value)));
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if inherited_key(field.name()) {
            self.kept
                .push((field.name().to_string(), Value::from(value)));
        }
    }
}

/// The inheritable keys of `span` and its ancestors, innermost first, duplicates dropped — so the closest spelling of
/// `request` or `workspace` is the one that travels, and a record never carries two values for one key.
fn inherited_from<'a, S>(span: &SpanRef<'_, S>) -> Vec<(String, Value)>
where
    S: for<'x> LookupSpan<'x>,
{
    let mut out: Vec<(String, Value)> = Vec::new();
    for ancestor in span.scope() {
        let extensions = ancestor.extensions();
        let Some(info) = extensions.get::<SpanInfo>() else {
            continue;
        };
        for (key, value) in &info.declared {
            if !out.iter().any(|(seen, _)| seen == key) {
                out.push((key.clone(), value.clone()));
            }
        }
    }
    out
}

/// The subscriber's whole behaviour: filter by target and level, then write to whichever sinks want the record.
struct DiagLayer {
    sink: Arc<Sink>,
}

impl<S> Layer<S> for DiagLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    /// The one place a disabled level is decided, so an event nobody asked for costs this compare and nothing else — no
    /// `format!`, no `Vec`, no lock. A per-target override can only raise what the ceiling check lets through, which is why the
    /// ceiling is the maximum over targets rather than the default level.
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        // **Spans are tracked whatever the level.** A `warn`-level run still has to be able to say which request a line
        // belonged to, and if the request span itself were filtered out at `info` the correlation key would vanish
        // exactly for the errors a reader most needs to place. What is gated is what gets *written*: `on_close` re-checks
        // the level before it emits a timing record, and events filter themselves above. The cost is one small extension
        // per phase — spans here are opened once per scan, embed run, search and request, never per item.
        if metadata.is_span() {
            return true;
        }
        self.sink
            .allows(metadata.target(), tracing_ordinal(metadata.level()))
    }

    /// Remember what a span declared and when it opened: the fields are read by every nested event, the instant only at
    /// close.
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut fields = SpanFields::default();
        attrs.record(&mut fields);
        if let Some(handle) = ctx.span(id) {
            handle.extensions_mut().insert(SpanInfo {
                declared: fields.kept,
                started: Instant::now(),
            });
        }
    }

    /// A closed phase reports what it cost (spec point 7). The record is the span's own — its target, a message naming
    /// it — plus the inherited keys an event inside it would have carried, so a timing line belongs to the request that
    /// caused the phase.
    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(handle) = ctx.span(&id) else {
            return;
        };
        let (target, name, ms, inherited) = {
            let extensions = handle.extensions();
            let Some(info) = extensions.get::<SpanInfo>() else {
                return;
            };
            // Saturating rather than overflowing: a phase that ran for six decades has the same diagnosis as one that
            // ran for 584 million years, and a log line is not where an overflow should be discovered.
            let ms = u64::try_from(info.started.elapsed().as_millis()).unwrap_or(u64::MAX);
            (
                handle.metadata().target(),
                handle.name().to_string(),
                ms,
                inherited_from(&handle),
            )
        };
        if !self
            .sink
            .allows(target, tracing_ordinal(&TracingLevel::INFO))
        {
            return;
        }
        let mut record = Record {
            msg: format!("{name} finished"),
            ..Record::default()
        };
        for (key, value) in inherited {
            record.push(&key, value);
        }
        record.push("elapsed_ms", Value::from(ms));
        if self.sink.stderr_on.load(Ordering::Relaxed) {
            self.sink.write_stderr(&record.human("info", target));
        }
        if self.sink.has_file.load(Ordering::Relaxed) {
            self.sink.write_file(&record.json("info", target));
        }
    }

    /// A field filled in after the span opened (`request` and `workspace` are declared empty and recorded once the
    /// request is parsed) merges into the same extension, so a nested event never sees half a correlation key.
    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(handle) = ctx.span(id) else {
            return;
        };
        let mut fields = SpanFields::default();
        values.record(&mut fields);
        if fields.kept.is_empty() {
            return;
        }
        let mut extensions = handle.extensions_mut();
        if let Some(info) = extensions.get_mut::<SpanInfo>() {
            for (key, value) in fields.kept {
                match info.declared.iter_mut().find(|(name, _)| *name == key) {
                    Some(slot) => slot.1 = value,
                    None => info.declared.push((key, value)),
                }
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut record = Record::default();
        event.record(&mut record);
        // **Correlation before formatting** (spec §"Serve"): whatever the enclosing spans declared rides on this
        // record, innermost first, and a field the event set itself is never overwritten — an emitter that named its own
        // workspace knows better than the span wrapped around it.
        if let Some(current) = ctx.event_span(event) {
            for (key, value) in inherited_from(&current) {
                if !record.fields.iter().any(|(name, _)| name == &key) {
                    record.push(&key, value);
                }
            }
        }
        let level = level_text(meta.level());
        // The cap is checked before the line is built (spec point 8): a suppressed diagnostic costs a counter, not a `format!`.
        if self.sink.stderr_on.load(Ordering::Relaxed) && !record.already_printed {
            let line = record.human(level, meta.target());
            self.sink.write_stderr(&line);
        }
        if self.sink.has_file.load(Ordering::Relaxed) {
            self.sink.write_file(&record.json(level, meta.target()));
        }
    }
}
