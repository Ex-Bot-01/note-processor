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
    write_would_block(io::stderr().as_raw_fd())
}

/// The same question for any descriptor, split out so the rule can be tested against a pipe this process controls instead
/// of against its own descriptor 2 — which no test may replace without affecting every other test in the binary.
#[cfg(unix)]
fn write_would_block(raw: std::os::fd::RawFd) -> bool {
    let mut fd = libc::pollfd {
        fd: raw,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: `fd` is one initialised `pollfd`, and the count passed matches it. `poll` writes only `revents`.
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    // Three answers, and only one of them means "write".
    //
    // * `ready == 0` is the timeout: the buffer is full and nobody is reading.
    // * An error condition — `POLLERR` on a broken pipe where the client went away, `POLLHUP`, `POLLNVAL` on a descriptor
    //   closed under us — also answers "do not write". Linux reports `POLLERR` **together with** `POLLOUT` in that case, so
    //   the error bits are checked first or they mean nothing. Writing anyway would only be pointless (`EPIPE` is discarded
    //   like any other error here), but a channel with no reader is not one to keep poking.
    // * A negative return means `poll` itself failed (an odd descriptor, or `EINTR`). That answers "would not block": the
    //   write below tolerates any error the descriptor can give, and a diagnostic that arrives beats one lost to a signal.
    if ready < 0 {
        return false;
    }
    if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return true;
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
    /// What `--log-spec` / `-v` asked for, kept verbatim for the second resolution.
    ///
    /// Levels are resolved twice — once before any configuration is read, so a run that refuses to load one still says where
    /// its diagnostics go, and once after, because `[log] level` cannot be known before the file is opened. The second pass
    /// has to resolve the **same three sources**, and the command line is one of them: re-resolving with `None` here silently
    /// dropped the flag, so `-vv` announced `debug from command line` and then ran at `warn` with stderr switched off — the
    /// flagship knob inert on every command that reads configuration, which is every command. Pinned by
    /// `a_verbose_flag_reaches_the_records_and_not_only_the_announce` and `a_flag_outranks_the_file_that_asked_first`, both of
    /// which assert records rather than the announce sentence for exactly that reason.
    flag: RwLock<Option<String>>,
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
            flag: RwLock::new(None),
            file: Mutex::new(None),
            file_attempted: AtomicBool::new(false),
        }
    }

    /// Keep what the command line asked for, verbatim, for the second resolution in [`apply_config`].
    fn remember_flag(&self, raw: Option<&str>) {
        *self.flag.write().expect("diagnostic flag spec lock") = raw.map(str::to_string);
    }

    /// The remembered `--log-spec` / `-v` text, if this run's command line wrote any.
    fn flag(&self) -> Option<String> {
        self.flag.read().expect("diagnostic flag spec lock").clone()
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

    /// Claim the right to print one diagnostic line on stderr, **before** anything is formatted for it.
    ///
    /// Two questions, in this order, neither of which touches a `String`:
    ///
    /// 1. Does the cap allow another line? A line past it costs a counter and nothing else — spec point 8 states that as a
    ///    design property, and the first version of this file only half honoured it: the cap was consulted after
    ///    [`Record::human`] had already built the line, so diagnostic thousand-and-one paid for the very formatting the cap
    ///    exists to avoid.
    /// 2. Will descriptor 2 take a write *now*? A client that captures stderr and never reads it is normal — a GUI that wants
    ///    to know whether anything went wrong does not parse the stream — and an anonymous pipe holds about 64 KiB. Measured
    ///    before this existed: 1 200 requests sent to a server whose stderr nobody read, **1 057 answered**, the child stalled
    ///    inside `write(2)`. A log channel that can stop the thing it is watching is not a log channel (see
    ///    [`stderr_would_block`] for what the question costs and why it is not `O_NONBLOCK`).
    ///
    /// A line refused by either question counts as suppressed, so the closing notice still appears and names both ways out.
    /// The file sink is unaffected by question two: a regular file does not fill.
    fn claim_stderr_line(&self) -> bool {
        if !self.stderr_on.load(Ordering::Relaxed) || !self.take_stderr_slot() {
            return false;
        }
        if stderr_would_block() {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Write a line whose slot was claimed by [`Sink::claim_stderr_line`].
    fn write_stderr_line(&self, line: &str) {
        // Written through a handle with the result dropped, for the reason `np_core::scan` gives for its own stderr receipt:
        // `eprintln!` panics when descriptor 2 refuses the write, which would replace a reportable failure with exit 101 and a
        // panic message that cannot be printed either.
        let mut err = io::stderr();
        let _ = writeln!(err, "{line}");
    }

    /// Whether the closing notice may be written without waiting. It is exempt from the cap — it is the line that explains the
    /// cap — but not from the pipe: writing it with a bare `writeln!` meant that a client which never drained stderr, in a run
    /// that had already dropped diagnostics (the exact state in which the notice is owed), left `np serve` answering every
    /// request and then **never exiting**: the last write of the process waited for a reader that would never come. Dropping
    /// it costs one sentence about a pipe nobody read.
    fn stderr_notice_fits(&self) -> bool {
        self.stderr_on.load(Ordering::Relaxed) && !stderr_would_block()
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
    // All three sources again, flag included: `[log] level` is what this pass exists to pick up, and picking it up by
    // dropping the command line would invert the documented precedence instead of honouring it.
    let flag = sink().flag();
    let env = match env_value() {
        Ok(value) => value,
        // Same shape as the refusal branch below: a diagnostics typo must not fail a run that is already past bootstrap, so
        // it warns and keeps the levels phase one settled on.
        Err(err) => {
            let mut err_out = io::stderr();
            let _ = writeln!(err_out, "np: {err}; keeping the log level already in force");
            return;
        }
    };
    let (spec, source) =
        match np_core::config::resolve_diag(flag.as_deref(), env.as_deref(), log_level(cfg)) {
            Ok(resolved) => resolved,
            // Defensive by the time bootstrap has had its say: every source that could be unreadable (`NP_LOG` as non-text,
            // `[log] level` as a bad spec) is screened above, and `Config::load` refuses a bad `[log] level` outright. Should a
            // value still arrive unparseable, this is the second phase — a command already at work must not be failed by a
            // diagnostics typo — so it warns and keeps the levels phase one settled on.
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
fn env_value() -> Result<Option<String>, String> {
    match std::env::var_os(DIAG_ENV_VAR) {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        // Text this program cannot read is refused, naming the variable — the rule `config::env_text` and `loglevel` both
        // follow. Treating it as unset would make "the environment did nothing" indistinguishable from "the environment was
        // nonsense", which is the invisibility the announce line exists to remove.
        Some(value) => value.into_string().map(Some).map_err(|_| {
            format!("{DIAG_ENV_VAR} is set to text this program cannot read; unset it, or write one of off, error, warn, info, debug, trace")
        }),
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
    // Phase two must resolve with this string or the flag is inert (see [`Sink::flag`]).
    sink().remember_flag(flag.as_deref());
    // The `[log]` table is **peeked** here — read leniently, ignoring a file that refuses for any other reason — because the
    // level announced on stderr has to be the answer that is *final*, and the file outranks the environment (§12 rule 7).
    // Without the peek, `NP_LOG=trace` next to `[log] level = "info"` would announce `trace from NP_LOG` and then quietly run
    // at info: precisely the invisibility the announce line exists to remove. The second benefit is the one spec point 3 asks
    // for — a config that refuses for an unrelated reason still says where its diagnostics go.
    let peek = np_core::config::peek_log_config();
    // A `[log] level` that cannot be parsed is **not this run's exit code**. The key sits in the section whose job is
    // reporting failures, so refusing it before the sinks exist is the one route by which a diagnostics typo stops a command:
    // measured, `np serve` with `level = "verbose"` exited 1 without answering a request and wrote nothing to the `[log] file`
    // the same table configured. Review Focus 1 asks for the opposite, and F33 already sets the precedent for the neighbouring
    // key — a bad `NP_LLAMA_LOG_LEVEL` refuses *per request* through `Config::load` while the server keeps running.
    //
    // So an unreadable file level is treated as unsaid: the run takes the levels it does understand (flag, `NP_LOG`, default),
    // still opens the file this table named, says once on stderr what it ignored, and lets `Config::load`'s strict refusal
    // arrive where it always has — a fault on each request, a `config-refused` record in that file. A bad `--log-spec` or
    // `NP_LOG` still refuses the run outright (`?` below): spec point 2 wants a mistyped command line to stop there, and only
    // there can it be corrected before any work starts.
    let file_level = match peek.level.as_deref() {
        None => None,
        Some(raw) => match np_core::config::parse_diag_spec(raw) {
            Ok(_) => Some(raw),
            Err(err) => {
                let mut err_out = io::stderr();
                let _ = writeln!(
                    err_out,
                    "np: {} ignored: {err}; the run continues and reports the refusal per request",
                    np_core::config::DIAG_CONFIG_KEY
                );
                None
            }
        },
    };
    let env = env_value()?;
    let (spec, source) =
        np_core::config::resolve_diag(flag.as_deref(), env.as_deref(), file_level)?;
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
    if suppressed == 0 || !shared.stderr_notice_fits() {
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
}

/// The bound on one path-bearing diagnostic field, in bytes, marker included.
///
/// A `what` is a path and a path can be long: a deep vault whose directory names say something reaches 512 bytes without
/// anybody trying. Unbounded, one file's name dictates the size of every record written about it and a terminal full of
/// 4 KB lines is a log nobody reads. The tempting fix is worse — redact the field when it gets silly — because "which
/// file?" is the whole reason the field exists.
///
/// So the bound clamps and **keeps the front**: paths diverge at the start rather than at the end, so what survives is the
/// vault and the directory, and `…` says the rest exists. 512 is neither generous nor mean: it holds every path a normal
/// vault produces (`PATH_MAX` is 4096, but real ones are tens of bytes) while keeping one record inside a screen.
const FIELD_BOUND_BYTES: usize = 512;

/// Which fields the bound applies to: the two that carry a caller's path. `msg` deliberately does **not** appear — its
/// length is program-authored, and clipping a TOML parse error mid-sentence removes the part that names the key to fix.
const FIELD_BOUND_KEYS: [&str; 2] = ["what", "workspace"];

/// Clamp one field value to [`FIELD_BOUND_BYTES`], marking that it was clamped.
fn clamp_field(name: &str, value: Value) -> Value {
    if !FIELD_BOUND_KEYS.contains(&name) {
        return value;
    }
    let Value::String(text) = value else {
        return value;
    };
    if text.len() <= FIELD_BOUND_BYTES {
        return Value::String(text);
    }
    // Byte lengths, then a step back to a character boundary: a vault on any filesystem can hold `große-notiz.md`, and a
    // cut through the middle of one of its characters would produce a path that names nothing and invalid UTF-8 besides.
    let mut keep = FIELD_BOUND_BYTES - '…'.len_utf8();
    while !text.is_char_boundary(keep) {
        keep -= 1;
    }
    let mut out = String::with_capacity(FIELD_BOUND_BYTES);
    out.push_str(&text[..keep]);
    out.push('…');
    Value::String(out)
}

impl Record {
    fn push(&mut self, name: &str, value: Value) {
        // The published key set is a promise about an immutable format, so an undocumented field is a programming mistake —
        // caught in every debug run (which is what the gate list runs) rather than in a reader's parser six months out.
        debug_assert!(
            RECORD_KEYS.contains(&name),
            "`{name}` is not in np's published diagnostic keys: {RECORD_KEYS:?}"
        );
        // Clamped here, at the one place a field enters a record, so stderr and the file cannot disagree about how a long
        // path was written — the two channels are read side by side when something is being explained.
        self.fields
            .push((name.to_string(), clamp_field(name, value)));
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
        let (target, name, ms) = {
            let extensions = handle.extensions();
            let Some(info) = extensions.get::<SpanInfo>() else {
                return;
            };
            // Saturating rather than overflowing: a phase that ran for six decades has the same diagnosis as one that
            // ran for 584 million years, and a log line is not where an overflow should be discovered.
            let ms = u64::try_from(info.started.elapsed().as_millis()).unwrap_or(u64::MAX);
            (handle.metadata().target(), handle.name().to_string(), ms)
        };
        // The level is asked before the record is assembled: `inherited_from` clones a `String` and a `Value` per correlation
        // key, and a span closing at a level nobody captures should not pay for either.
        if !self
            .sink
            .allows(target, tracing_ordinal(&TracingLevel::INFO))
        {
            return;
        }
        let inherited = inherited_from(&handle);
        let mut record = Record {
            msg: format!("{name} finished"),
            ..Record::default()
        };
        for (key, value) in inherited {
            record.push(&key, value);
        }
        record.push("elapsed_ms", Value::from(ms));
        if self.sink.claim_stderr_line() {
            self.sink.write_stderr_line(&record.human("info", target));
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
        // Claimed before the line is built (spec point 8): a diagnostic that will not be printed costs a counter and a poll,
        // not a `format!`.
        if self.sink.claim_stderr_line() {
            let line = record.human(level, meta.target());
            self.sink.write_stderr_line(&line);
        }
        if self.sink.has_file.load(Ordering::Relaxed) {
            self.sink.write_file(&record.json(level, meta.target()));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::write_would_block;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};

    /// A fresh pipe whose write end is non-blocking, so a test can fill it without the harness itself waiting.
    fn pipe() -> (std::fs::File, std::fs::File) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: two ints that `pipe2` fills; close-on-exec keeps them out of any child this process spawns.
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0,
            "test setup: pipe2"
        );
        // SAFETY: both numbers are descriptors the call above just created, each adopted by exactly one owner here.
        let (read_half, write_half) = unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        };
        // SAFETY: `write_half` is live; reading its flags to add one bit changes nothing else about it.
        let flags = unsafe { libc::fcntl(write_half.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "test setup: F_GETFL");
        assert!(
            unsafe {
                libc::fcntl(
                    write_half.as_raw_fd(),
                    libc::F_SETFL,
                    flags | libc::O_NONBLOCK,
                )
            } >= 0,
            "test setup: O_NONBLOCK"
        );
        (read_half, write_half)
    }

    /// The rule the whole non-blocking stderr design rests on, measured against a pipe this test owns rather than against
    /// descriptor 2 — whose capacity nobody may shrink from inside the process that shares it.
    #[test]
    fn a_pipe_with_room_is_ready_and_one_nobody_reads_is_not() {
        let (mut read_half, write_half) = pipe();
        let mut write_half = write_half;
        assert!(
            !write_would_block(write_half.as_raw_fd()),
            "an empty pipe takes writes; a guard that says otherwise would drop every diagnostic in existence"
        );

        // Filled to whatever the kernel allows rather than to an assumed size: capacity is tunable, and a test that guessed
        // 64 KiB would fail on a machine that permits more.
        let filler = vec![b'x'; 8 * 1024];
        let mut written = 0usize;
        loop {
            match write_half.write(&filler) {
                Ok(n) => written += n,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("test setup: filling the pipe: {err}"),
            }
        }
        assert!(written > 0, "test setup: a pipe that takes no bytes at all");
        assert!(
            write_would_block(write_half.as_raw_fd()),
            "after {written} bytes with nobody reading, `poll` must say not-now — that answer is what keeps `np serve` moving"
        );

        // And the answer follows the reader: drain it and the same descriptor is ready again.
        let mut drained = vec![0u8; written];
        read_half
            .read_exact(&mut drained)
            .expect("test setup: draining the pipe");
        assert!(
            !write_would_block(write_half.as_raw_fd()),
            "a reader that catches up must unblock the channel, not leave it permanently dropped"
        );
    }

    /// A client that goes away entirely (the read end closed, so the pipe is broken) is answered the same way as one that
    /// merely stopped reading: do not write. Writing would not crash — `EPIPE` is discarded like any other error here — but a
    /// channel with no reader is not one to keep poking, and Linux reports `POLLERR` alongside `POLLOUT`, so checking the error
    /// bits first is what makes this answer mean anything.
    #[test]
    fn a_broken_pipe_is_not_ready() {
        let (read_half, write_half) = pipe();
        drop(read_half);
        assert!(
            write_would_block(write_half.as_raw_fd()),
            "a closed reader must not read as writable"
        );
    }

    /// The everyday case must stay untouched: a terminal and `/dev/null` answer ready, so `-v` at a shell prompt never
    /// loses a line to the guard.
    #[test]
    fn a_character_device_is_ready() {
        let null = std::fs::File::open("/dev/null").expect("test setup: /dev/null");
        assert!(
            !write_would_block(null.as_raw_fd()),
            "a sink that always accepts writes must never be treated as blocked"
        );
    }
}
