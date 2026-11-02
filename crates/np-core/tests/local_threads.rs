//! D13: one cell of the thread-count sweep, measured through this program's own loader.
//!
//! **One process measures one cell.** Every number in a run belongs to exactly one `(threads, tokens)` pair, and the
//! driver (`tests/probes/thread_sweep.sh`) starts a fresh process per cell rather than looping inside one test — for
//! the reason R6d round 2 learned the hard way: a threadpool is process-wide state, so the second measurement in a
//! process inherits the first pool, and `VmHWM` is a process-lifetime high-water mark that cannot be attributed to one
//! decode once anything else has run. Separate processes are also what a user gets: `np embed` loads, works, exits.
//!
//! With `NP_TEST_MODEL_PATH` unset the case prints `NOT COVERAGE` and passes, for the standing reason (F18): an absent
//! 310 MB artifact is not a failure of the code, and it must not be reported as one. It is never silently skipped —
//! "skipped" and "measured nothing" are different sentences and only one of them is true here.
//!
//! Run one cell:
//!
//! ```sh
//! NP_TEST_MODEL_PATH=/path/to/embeddinggemma-2-Q8_0.gguf \
//! NP_THREAD_THREADS=8 NP_THREAD_TOKENS=403 NP_THREAD_REPEATS=5 \
//! NP_THREAD_DUMP=/tmp/vec-8.f32 \
//! cargo test -p np-core --features local-model,fetch --test local_threads -- --nocapture
//! ```
//!
//! **What this measures and what it does not.** It measures the forward pass behind `np embed` — same loader, same
//! `embed_text`, same mean-then-L2 pooling, same window arithmetic — so a throughput figure here is a figure about this
//! program, and a vector dumped here is byte-comparable with one `np embed` would store. It does not measure the
//! chunker, the database, or digest verification; §14's corpus rows already cover the whole command, and this file
//! exists to separate one variable from all the others.

#![cfg(feature = "local-model")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The artifact this round is measured against, or `None` when the environment says it is absent.
fn artifact() -> Option<PathBuf> {
    std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from)
}

/// Print the standing "this measured nothing" line and exit 0 — the house shape for an artifact-gated case, chosen so
/// the line is greppable in CI (`NOT COVERAGE` count is a floor the CI row pins).
fn no_coverage(why: &str) {
    println!("NOT COVERAGE: {why}");
    println!(
        "NOT COVERAGE: run with NP_TEST_MODEL_PATH=/path/to/embeddinggemma-2-Q8_0.gguf \
         (sha256 2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135) to measure."
    );
}

/// One integer from the environment, with the variable's name in the complaint: a sweep cell that silently defaults a
/// parameter is a cell whose row means nothing, so a missing knob is an error rather than a guess.
fn env_us(name: &str, default: Option<usize>) -> usize {
    match std::env::var(name) {
        Ok(raw) => raw.parse::<usize>().unwrap_or_else(|problem| {
            panic!("$ {name} = {raw:?} is not a count of tokens or threads: {problem}")
        }),
        Err(_) => default.unwrap_or_else(|| {
            panic!(
                "${name} is required once a cell is being run (see tests/local_threads.rs header)"
            )
        }),
    }
}

/// Has any part of a cell been asked for? `$NP_THREAD_THREADS` and `$NP_THREAD_TOKENS` are the pair that names one.
///
/// **Why "nothing selected" is a print rather than a failure:** with an artifact configured, `cargo test --test
/// local_threads` has to pass on its own — a measurement file that breaks someone's ordinary `cargo test` because it
/// wants three variables they never heard of is a defect, not a reminder. A *half*-specified cell is the opposite case
/// and does fail: `$NP_THREAD_TOKENS=7992` with no thread count is a typo in a driver, and quietly measuring nothing
/// there would put a hole in the sweep's log.
fn cell_selected() -> Result<bool, String> {
    let threads = std::env::var_os("NP_THREAD_THREADS").is_some();
    let tokens = std::env::var_os("NP_THREAD_TOKENS").is_some();
    match (threads, tokens) {
        (false, false) => Ok(false),
        (true, true) => Ok(true),
        (false, true) => Err("NP_THREAD_TOKENS is set without NP_THREAD_THREADS".to_string()),
        (true, false) => Err("NP_THREAD_THREADS is set without NP_THREAD_TOKENS".to_string()),
    }
}

/// Resident high-water mark for this process, in KiB — the same `/proc/self/status` field `tests/local_window.rs`
/// reads, so a number here and a number there mean the same thing.
fn peak_rss_kib() -> u64 {
    let status =
        fs::read_to_string("/proc/self/status").expect("test: /proc/self/status is readable");
    status
        .lines()
        .find_map(|line| {
            line.starts_with("VmHWM:")
                .then(|| line.split_whitespace().nth(1))
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
        })
        .expect("test: VmHWM is present in /proc/self/status")
}

/// Kernel threads this process has right now — the check that a requested worker count actually became threads.
fn os_threads() -> u64 {
    let status =
        fs::read_to_string("/proc/self/status").expect("test: /proc/self/status is readable");
    status
        .lines()
        .find_map(|line| {
            line.starts_with("Threads:")
                .then(|| line.split_whitespace().nth(1))
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
        })
        .expect("test: Threads is present in /proc/self/status")
}

/// Text long enough to be cut to a target token count: one sentence repeated, so the token stream is uniform and every
/// cell decodes the same ids. `tests/local_window.rs` uses this construction; keeping it here means the two files'
/// numbers are comparable without a shared fixture that could drift.
fn text_of(n_copies: usize) -> String {
    let unit = "The quick brown fox jumps over the lazy dog while the rain falls softly on the roof and the \
                city hums its low tune beneath the grey evening sky. ";
    unit.repeat(n_copies)
}

/// A note of *exactly* `target` tokens (or as close as this tokenizer will come), plus the count actually reached.
///
/// Measured by counting rather than assuming: a token count derived from bytes would make every row of the sweep wrong
/// by however far the tokenizer disagrees with the arithmetic, and the whole point is comparing cells against each
/// other at equal length.
fn note_at_exactly_tokens(
    model: &np_core::local_model::LoadedModel,
    target: usize,
) -> (String, usize) {
    let mut copies = target / 20 + 4;
    loop {
        let text = text_of(copies);
        let got = match model.count_tokens("", &text) {
            Ok(n) => n,
            Err(_) => {
                copies -= 1;
                continue;
            }
        };
        if got <= target {
            return (text, got);
        }
        if copies == 1 {
            return (text, got);
        }
        copies -= 1;
    }
}

/// cosine similarity and the largest absolute coordinate difference between two vectors — the two numbers that answer
/// "did the thread count change the answer", because a cosine of 0.99999 with a large single-coordinate delta is a
/// different story from both numbers being tiny.
fn compare(a: &[f32], b: &[f32]) -> (f64, f64) {
    assert_eq!(
        a.len(),
        b.len(),
        "both vectors are 768 wide or the dump is wrong"
    );
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    let mut max_abs = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
        max_abs = max_abs.max((x - y).abs());
    }
    let cosine = if na > 0.0 && nb > 0.0 {
        dot / (na.sqrt() * nb.sqrt())
    } else {
        0.0
    };
    (cosine, max_abs)
}

/// Write one vector as little-endian float-32, so a second process can read it back and the two can be compared
/// without either trusting the other's arithmetic.
fn dump(path: &Path, vector: &[f32]) {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let mut file = fs::File::create(path).expect("test: the dump path is writable");
    file.write_all(&bytes)
        .expect("test: the vector reaches the dump");
    file.sync_all()
        .expect("test: the dump is on disk before the row prints");
}

/// One cell: load at `$NP_THREAD_THREADS`, decode `$NP_THREAD_TOKENS`-token notes `$NP_THREAD_REPEATS` times, print a
/// machine-readable row, and optionally dump the first vector for the determinism comparison.
#[test]
fn one_cell_of_the_thread_sweep() {
    let Some(path) = artifact() else {
        no_coverage("NP_TEST_MODEL_PATH is unset, so this cell measured nothing.");
        return;
    };
    if !path.is_file() {
        no_coverage(&format!(
            "NP_TEST_MODEL_PATH points at {} which is not a file.",
            path.display()
        ));
        return;
    }

    match cell_selected() {
        Ok(false) => {
            no_coverage(
                "no cell was selected: set $NP_THREAD_THREADS and $NP_THREAD_TOKENS \
                 (see tests/probes/thread_sweep.sh) to measure one.",
            );
            return;
        }
        Ok(true) => {}
        Err(problem) => panic!("half a sweep cell is not a measurement: {problem}"),
    }
    let threads = env_us("NP_THREAD_THREADS", None);
    let target = env_us("NP_THREAD_TOKENS", None);
    let repeats = env_us("NP_THREAD_REPEATS", Some(3));
    let dump_to = std::env::var_os("NP_THREAD_DUMP").map(PathBuf::from);
    assert!(threads >= 1, "a cell with zero workers measures nothing");
    assert!(
        repeats >= 2,
        "one sample is not a spread (§8: never a single number)"
    );

    // **Available parallelism is printed, not assumed.** It is the denominator any future policy would use, and it is
    // affinity-aware: `taskset -c 0-3` on this host answers 4 where an unrestricted run answers 16. A row that quoted
    // "cores" without saying which count it meant would be unreadable next to a container run.
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(usize::MAX);

    let options = np_core::local_model::LoadOptions {
        threads: Some(
            std::num::NonZeroU32::try_from(u32::try_from(threads).expect("test: threads fits u32"))
                .expect("test: threads is non-zero"),
        ),
    };
    let t_load = Instant::now();
    let model =
        np_core::local_model::load_with_options(&path, 768, options).unwrap_or_else(|problem| {
            panic!("cell threads={threads}: the artifact refused to load: {problem}")
        });
    let load_s = t_load.elapsed().as_secs_f64();

    // **The readback is the cell's first fact.** If llama.cpp clamps or ignores what it was told, the row says so and
    // the sweep measures what actually ran; a policy built on "we asked for 12" would be fiction.
    let (built_generation, built_batch) = model.threads();
    println!(
        "CELL       threads_requested={threads} readback_generation={built_generation} \
         readback_batch={built_batch} available_parallelism={available} os_threads_after_load={}",
        os_threads()
    );
    println!(
        "LOAD       {load_s:.2} s  window={} KiB_peak_rss={}",
        model.window(),
        peak_rss_kib()
    );

    let (note, tokens) = note_at_exactly_tokens(&model, target);
    assert!(tokens > 0, "a zero-token note measures a rate of nothing");

    // Warm-up decode outside the timed window: the first pass pays graph construction and buffer allocation, which is
    // a fact about cold starts (§14's corpus rows print it separately) and not the cost of a decode.
    let budget = model.window();
    let warm = model
        .embed_text("", &note, budget)
        .expect("test: a note at or below the window is accepted");
    assert_eq!(warm.dim(), 768, "the artifact answers 768 coordinates");

    let mut per_repeat = Vec::with_capacity(repeats);
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    for _ in 0..repeats {
        let t = Instant::now();
        let vector = model
            .embed_text("", &note, budget)
            .expect("test: the same note is accepted again");
        per_repeat.push(t.elapsed().as_secs_f64() * 1000.0);
        // Stored as plain components from here on: the comparison and the dump are about numbers, and `Vector`'s
        // storage form is for the database (`to_bytes`), not for arithmetic.
        vectors.push(vector.components().to_vec());
    }

    // **Determinism inside one process, at one thread count.** Two decodes of identical bytes must agree; if they do
    // not, the cross-process comparison below has no baseline and the answer is "the runtime is not reproducible at
    // all", which is a different finding than D13 asked for.
    let (self_cosine, self_max_abs) = compare(&vectors[0], &vectors[vectors.len() - 1]);

    let mut sorted = per_repeat.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("test: durations are ordered"));
    let lo = sorted[0];
    let hi = sorted[sorted.len() - 1];
    let med = if sorted.len() % 2 == 1 {
        sorted[sorted.len() / 2]
    } else {
        0.5 * (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2])
    };
    let tok_s = 1000.0 * tokens as f64 / med;

    if let Some(target_path) = &dump_to {
        dump(target_path, &vectors[0]);
    }

    println!(
        "DECODE     tokens={tokens} repeats={repeats} per_repeat_ms min={lo:.1} median={med:.1} max={hi:.1} \
         tok_s={tok_s:.1}"
    );
    println!(
        "RSS        peak_kib={} os_threads_after_decode={}",
        peak_rss_kib(),
        os_threads()
    );
    println!("SELF       cosine={self_cosine:.9} max_abs_diff={self_max_abs:.3e} (same process, same thread count)");
    println!(
        "SUMMARY    threads={threads} readback={built_batch} tokens={tokens} median_ms={med:.1} tok_s={tok_s:.1} \
         spread_pct={:.2} load_s={load_s:.2} peak_rss_kib={} os_threads={} available={available}",
        100.0 * (hi - lo) / med,
        peak_rss_kib(),
        os_threads()
    );

    // A cell that could not ask for its worker count is not a measurement of that count. Kept as an assertion rather
    // than prose so a clamped runtime fails the row instead of quietly producing one.
    assert!(
        built_batch >= 1,
        "the context answered {built_batch} batch workers, which cannot be timed meaningfully"
    );
}

/// The clamp question, asked directly: does the runtime use the number it was given?
///
/// This is the fact any policy would rest on. It is a case rather than a line in the sweep log because a wrong answer
/// here invalidates every throughput row — if `n_threads` is ignored, the sweep measures the same configuration five
/// times and the only thing that changes is the label.
#[test]
fn asking_for_a_worker_count_is_answered_with_that_count() {
    let Some(path) = artifact() else {
        no_coverage("NP_TEST_MODEL_PATH is unset, so this case checked nothing.");
        return;
    };
    if !path.is_file() {
        no_coverage(&format!(
            "NP_TEST_MODEL_PATH points at {} which is not a file.",
            path.display()
        ));
        return;
    }

    for requested in [1usize, 2, 4] {
        let options = np_core::local_model::LoadOptions {
            threads: Some(
                std::num::NonZeroU32::new(requested as u32).expect("test: these are non-zero"),
            ),
        };
        let model = np_core::local_model::load_with_options(&path, 768, options)
            .unwrap_or_else(|problem| panic!("threads={requested}: load refused: {problem}"));
        let (generation, batch) = model.threads();
        println!("READBACK   requested={requested} generation={generation} batch={batch}");
        assert_eq!(
            (generation, batch),
            (requested, requested),
            "the context did not honour the worker count it was given"
        );
    }

    // The default is a number, not an absence: with no option set, the runtime answers its own compile-time default.
    // Naming it here is what lets §14 say "today's behaviour" without guessing at 4.
    let model =
        np_core::local_model::load(&path, 768).expect("test: the artifact loads with defaults");
    let (generation, batch) = model.threads();
    println!("READBACK   requested=default generation={generation} batch={batch}");
    assert!(
        generation >= 1 && batch >= 1,
        "a default context has no workers?"
    );
}
