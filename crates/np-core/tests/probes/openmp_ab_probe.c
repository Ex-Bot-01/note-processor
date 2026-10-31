/*
 * The OpenMP A/B: the same pinned llama.cpp built twice, one forward pass through each, and the difference in
 * wall-clock and memory printed rather than argued about.
 *
 * Why a separate probe when `decode_probe.c` exists: that file documents itself as *not* measuring wall time
 * (A2 review P2-7 — throughput claims belong to the Rust test), and this one's whole purpose is throughput under
 * two configurations. Leaving the old probe's contract intact keeps every §14 citation beside the code that
 * produced it.
 *
 * WHAT THIS IS AND IS NOT COMPARING. The two sides differ by a cmake option (`GGML_OPENMP=OFF` vs `ON`), which means
 * they also differ by compiler flags, by link dependencies (`-fopenmp`, libgomp at runtime) and by the wait strategy of
 * whichever library owns the worker threads. So this is an A/B of two **build configurations**, not of "the flag in
 * isolation" — the honest reading is "does shipping OpenMP change what a user experiences", which happens to be the
 * question we actually have. What it cannot do is attribute a difference to OpenMP's code generation alone.
 *
 * What it prints, one line per fact:
 *
 *   BUILD      `llama_version()` plus whether libgomp is actually mapped into this process — read out of
 *              `/proc/self/maps`, so "the ON build is the ON build" is evidence from the binary rather than a claim
 *              about a cmake flag. Version alone cannot tell the two apart: `LLAMA_BUILD_IS_DEV=ON` makes every
 *              tree between v0.8.0 and v0.8.1 report `0.8.0-dev`, which is why `third_party/llama-cpp.toml` pins a
 *              header digest instead.
 *   LOAD       wall time to load the artifact *and build a context*, the change in resident set, and the change in
 *              `/proc/self/io` read_bytes. This is what answered §9's mmap question (2026-10-31): with the artifact
 *              evicted from our page-cache view, `read_bytes` moves by 309,858,304 B — more than the file's own
 *              309,855,456 B, because that counter is page-granular and reads ahead — so an mmap-first load pulls the
 *              whole artifact through the kernel on first use. `--cold` performs that eviction (`posix_fadvise`
 *              DONTNEED) before the baseline is taken; without it the pages are already resident and the counter moves
 *              by nothing. Note the LOAD window includes context creation, so its warm/cold difference is an upper
 *              bound on the cost of reading, not a clean measurement of it.
 *   DECODE     token count actually decoded, wall time, tokens/s, and per-repeat min/median/max — the last three
 *              because a single average over N repeats hides drift, and one process decoding eight times is *not*
 *              eight measurements. Resident set before and after, plus the process-lifetime high-water mark (`VmHWM`,
 *              which therefore includes the ~387 MB of load and is not attributable to the forward pass on its own).
 *              Thread evidence is printed rather than inferred: `n_threads`/`n_threads_batch` as the context was told,
 *              CPU utilisation from this process's own utime+stime across the decode window, and the kernel's
 *              `Threads:` count sampled after the last decode. Expect that last one to read 4 with OpenMP and 1
 *              without — libgomp keeps its team, ggml builds a pool per graph plan and tears it down — which is
 *              exactly why utilisation, not thread count, is the figure that says how many cores worked.
 *   SUMMARY    one machine-readable line at the end, so a table in `docs/` can be tied to the run that produced it.
 *              The committed log beside this file (`openmp-ab-2026-10-31.log`) is the raw evidence for §14's table; if
 *              you change the probe, re-run and re-commit it rather than editing numbers.
 *
 * Comparability: both sides decode **identical token ids** — the sequence is generated from a repeated sentence and
 * truncated to exactly `tokens`, after a floor check, before either timer starts. A highly repetitive input does not
 * change dense-FFN or attention FLOPs, so it is a fair load to compare across builds even though its shape is not a
 * note's (production chunks are ~403 tokens; 7,992 exists here to price the ceiling, not to imitate a document).
 * Repeat counts do NOT make a sample: throughput claims in `docs/` must cite several *processes* per cell, which is
 * what the committed matrix below does.
 *
 * Measured with it on 2026-10-31 — this host (16 CPUs, 7,419 MB RAM), the pinned Q8_0 artifact, one process per run on an
 * otherwise idle machine, 5 processes per cell at 403 tokens and 3 at 7,992. Medians taken ACROSS processes:
 *
 *   403  warm  OFF 156.3 tok/s (155.8-156.8)  ON 156.0 (155.2-156.6)   -0.19 %
 *   403  cold  OFF 155.8 tok/s (155.4-157.2)  ON 156.0 (155.6-156.8)   +0.13 %
 *   7992 cold   OFF  81.4 tok/s ( 81.2- 81.8) ON  80.9 ( 80.7- 81.7)   -0.61 %
 *   7992 warm   OFF  81.5 tok/s ( 81.2- 81.5) ON  81.1 ( 81.1- 81.7)   -0.49 %
 *
 * every difference smaller than the spread between two processes running the SAME build, and cpu_utilisation at 396-400 %
 * on both sides. So OpenMP bought nothing and cost a libgomp link dependency plus ~180-290 kB of peak RSS; OFF stays.
 * The raw log is committed beside this file as `openmp-ab-2026-10-31.log`, and §14 of
 * `docs/design-m2-local-embeddings.md` holds the table, the scope limits and what would reopen the decision. The thread
 * count — not the threading library — is D13 in `docs/backlog-m2-followups.md`.
 *
 * Build (archives from a cmake configure of the vendored tree; see `decode_probe.c` for the same shape):
 *
 *   gcc -O2 -o /tmp/ab_off crates/np-core/tests/probes/openmp_ab_probe.c \
 *     -I .vendor/llama.cpp-v0.8.0/include -I .vendor/llama.cpp-v0.8.0/ggml/include \
 *     "$OFF"/src/libllama.a "$OFF"/ggml/src/libggml.a "$OFF"/ggml/src/libggml-cpu.a \
 *     "$OFF"/ggml/src/libggml-base.a -lstdc++ -lm -lpthread -ldl
 *
 *   # The OpenMP configuration needs the runtime at link time, which is itself part of what "ON" costs:
 *   gcc -O2 -fopenmp -o /tmp/ab_on ... "$ON"/src/libllama.a ... -lstdc++ -lm -lpthread -ldl
 *
 * Run — the matrix that produced the committed log, one process per line, never overlapping (this probe measures a
 * machine doing one thing), and `$M` the pinned artifact:
 *
 *   for cell in "403 8" "7992 1"; do set -- $cell
 *     for i in 1 2 3 4 5; do                       # 5 processes per cell at 403, 3 at 7,992
 *       for v in off on; do for mode in warm cold; do
 *         [ "$mode" = cold ] && flag=--cold || flag=
 *         /tmp/ab_$v "$M" 8192 $1 $2 $flag
 *       done; done; done; done
 *
 * Like its sibling, a sequence longer than the context aborts the process (`GGML_ASSERT` under NDEBUG); `nctx` is
 * passed as `n_ctx = n_batch = n_ubatch`, which is what `local_model::load` asks for.
 */
#define _GNU_SOURCE 1
#include "llama.h"
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

/* A /proc read that fails must not become a zero in the table: every caller aborts on -1. */
static long status_kb(const char * field) {
    FILE * f = fopen("/proc/self/status", "r");
    if (!f) return -1;
    char line[256];
    long kb = -1;
    size_t len = strlen(field);
    while (fgets(line, sizeof(line), f)) {
        if (!strncmp(line, field, len)) { kb = atol(line + len); break; }
    }
    fclose(f);
    return kb;
}

static long status_long(const char * field) {
    FILE * f = fopen("/proc/self/status", "r");
    if (!f) return -1;
    char line[256];
    long value = -1;
    size_t len = strlen(field);
    while (fgets(line, sizeof(line), f)) {
        if (!strncmp(line, field, len)) { value = atol(line + len); break; }
    }
    fclose(f);
    return value;
}

/* Bytes this process has caused the kernel to read from storage, page-granular. */
static long long io_read_bytes(void) {
    FILE * f = fopen("/proc/self/io", "r");
    if (!f) return -1;
    char line[256];
    long long bytes = -1;
    while (fgets(line, sizeof(line), f)) {
        if (!strncmp(line, "read_bytes:", 11)) { bytes = atoll(line + 11); break; }
    }
    fclose(f);
    return bytes;
}

/* CPU time this process has spent in user+system, in clock ticks (utime + stime from /proc/self/stat). */
static long long cpu_ticks(void) {
    FILE * f = fopen("/proc/self/stat", "r");
    if (!f) return -1;
    char buf[4096];
    size_t n = fread(buf, 1, sizeof(buf) - 1, f);
    fclose(f);
    if (n == 0) return -1;
    buf[n] = '\0';
    /* Field 2 is the command in parentheses and may contain spaces; count fields from the last ')'. */
    char * close = strrchr(buf, ')');
    if (!close) return -1;
    long long utime = -1, stime = -1;
    /* Note the leading space: `%c` does not skip whitespace, and field 3 (state) is separated from the ')' by one. */
    if (sscanf(close + 1,
               " %*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %lld %lld",
               &utime, &stime) != 2) return -1;
    return utime + stime;
}

/* Is libgomp mapped into this process? The only honest way to say which build ran. */
static int gomp_mapped(void) {
    FILE * f = fopen("/proc/self/maps", "r");
    if (!f) return -1;
    char line[1024];
    int found = 0;
    while (fgets(line, sizeof(line), f)) {
        if (strstr(line, "gomp")) { found = 1; break; }
    }
    fclose(f);
    return found;
}

static double seconds_since(struct timespec * start) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (double)(now.tv_sec - start->tv_sec) + (double)(now.tv_nsec - start->tv_nsec) / 1e9;
}

static int compare_double(const void * a, const void * b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : (x > y ? 1 : 0);
}

/* llama.cpp is chatty at info level (one line per graph tensor); a throughput probe wants its own lines to be the
 * readable part of the run, so only warnings and errors reach stderr. */
static void log_filter(enum ggml_log_level level, const char * text, void * user_data) {
    (void)user_data;
    if (level >= GGML_LOG_LEVEL_WARN) fputs(text, stderr);
}

int main(int argc, char ** argv) {
    if (argc < 5) {
        fprintf(stderr, "usage: %s <gguf> <nctx> <tokens> <repeats> [--cold]\n", argv[0]);
        return 2;
    }
    const char * path = argv[1];
    int nctx = atoi(argv[2]);
    int target = atoi(argv[3]);
    int repeats = atoi(argv[4]);
    int cold = argc > 5 && !strcmp(argv[5], "--cold");

    struct timespec t_start;
    clock_gettime(CLOCK_MONOTONIC, &t_start);
    llama_log_set(log_filter, NULL);

    struct stat st;
    if (stat(path, &st) != 0) { fprintf(stderr, "cannot stat %s\n", path); return 1; }

    printf("BUILD      llama_version=%s gomp_mapped=%s variant=%s\n",
           llama_version(), gomp_mapped() ? "yes" : "no", gomp_mapped() > 0 ? "on" : "off");

    if (cold) {
        int fd = open(path, O_RDONLY);
        if (fd >= 0) {
            posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
            close(fd);
        }
    }

    long rss_before = status_kb("VmRSS:");
    long threads_before = status_long("Threads:");
    long long read_before = io_read_bytes();
    if (rss_before < 0 || threads_before < 0 || read_before < 0) {
        fprintf(stderr, "cannot read /proc/self — refusing to print a zero\n");
        return 1;
    }
    struct timespec t_load;
    clock_gettime(CLOCK_MONOTONIC, &t_load);

    struct llama_model_params mparams = llama_model_default_params();
    struct llama_model * model = llama_model_load_from_file(path, mparams);
    if (!model) { fprintf(stderr, "load failed for %s\n", path); return 1; }

    struct llama_context_params cparams = llama_context_default_params();
    cparams.n_ctx = (uint32_t)nctx;
    cparams.n_batch = (uint32_t)nctx;
    cparams.n_ubatch = (uint32_t)nctx;
    struct llama_context * ctx = llama_init_from_model(model, cparams);
    if (!ctx) { fprintf(stderr, "context failed\n"); return 1; }

    double load_seconds = seconds_since(&t_load);
    long rss_loaded = status_kb("VmRSS:");
    long long read_after_load = io_read_bytes();
    if (rss_loaded < 0 || read_after_load < 0) { fprintf(stderr, "cannot read /proc/self\n"); return 1; }
    printf("LOAD       %.2f s  n_ctx_requested=%d built_n_ctx=%d n_threads=%d n_threads_batch=%d  (window includes context creation)\n",
           load_seconds, nctx, llama_n_ctx(ctx), llama_n_threads(ctx), llama_n_threads_batch(ctx));
    printf("LOAD RSS   before=%ld kB after=%ld kB delta=%ld kB\n", rss_before, rss_loaded, rss_loaded - rss_before);
    printf("LOAD IO    read_bytes_delta=%lld bytes (page-granular; file on disk=%lld bytes, llama_model_size=%llu bytes)\n",
           read_after_load - read_before, (long long)st.st_size, (unsigned long long)llama_model_size(model));

    /* A sequence of exactly `target` tokens: repeat a sentence, then truncate. The same token ids in both builds, and
     * generated before either timer starts, so a throughput difference cannot belong to the input. */
    const char * unit = "The quick brown fox jumps over the lazy dog while the rain falls softly on the roof. ";
    size_t unit_len = strlen(unit);
    size_t buf_len = (size_t)(target + 4) * unit_len;
    char * buf = malloc(buf_len + 1);
    if (!buf) { fprintf(stderr, "OOM\n"); return 1; }
    for (size_t off = 0; off < buf_len; off += unit_len) memcpy(buf + off, unit, unit_len);
    buf[buf_len] = '\0';

    /* v0.8.0's `llama_tokenize` cannot size a buffer by being passed NULL, so the upper bound is the character
     * count: a BPE never emits more tokens than it is given characters. */
    const struct llama_vocab * vocab = llama_model_get_vocab(model);
    int room = (int)(buf_len + 8);
    llama_token * tokens = malloc((size_t)room * sizeof(llama_token));
    if (!tokens) { fprintf(stderr, "OOM\n"); return 1; }
    int ntok = llama_tokenize(vocab, buf, (int)buf_len, tokens, room, true, false);
    if (ntok < 0) { fprintf(stderr, "tokenize failed (%d)\n", ntok); return 1; }
    if (ntok < target) { fprintf(stderr, "only %d tokens available for a target of %d\n", ntok, target); return 1; }
    ntok = target;

    double * per_repeat = malloc((size_t)(repeats > 0 ? repeats : 1) * sizeof(double));
    if (!per_repeat) { fprintf(stderr, "OOM\n"); return 1; }

    long rss_decode_before = status_kb("VmRSS:");
    long long cpu_before = cpu_ticks();
    struct timespec t_decode;
    clock_gettime(CLOCK_MONOTONIC, &t_decode);
    int rc = -1;
    for (int i = 0; i < repeats; i++) {
        struct timespec t_each;
        clock_gettime(CLOCK_MONOTONIC, &t_each);
        struct llama_batch batch = llama_batch_get_one(tokens, ntok);
        rc = llama_decode(ctx, batch);
        per_repeat[i] = seconds_since(&t_each) * 1000.0;
        if (rc != 0) break;
    }
    double decode_seconds = seconds_since(&t_decode);
    long threads_during = status_long("Threads:");
    long long cpu_after = cpu_ticks();
    long rss_decode_after = status_kb("VmRSS:");
    if (rss_decode_before < 0 || rss_decode_after < 0 || threads_during < 0 || cpu_before < 0 || cpu_after < 0) {
        fprintf(stderr, "cannot read /proc/self around the decode window\n");
        return 1;
    }

    int done = rc == 0 ? repeats : 0;
    double lo = 0.0, med = 0.0, hi = 0.0;
    if (done > 0) {
        qsort(per_repeat, (size_t)done, sizeof(double), compare_double);
        lo = per_repeat[0];
        hi = per_repeat[done - 1];
        med = done % 2 ? per_repeat[done / 2] : 0.5 * (per_repeat[done / 2 - 1] + per_repeat[done / 2]);
    }
    double cpu_pct = decode_seconds > 0
        ? (double)(cpu_after - cpu_before) / sysconf(_SC_CLK_TCK) / decode_seconds * 100.0 : 0.0;

    printf("DECODE     rc=%d tokens=%d repeats=%d/%d wall=%.2f s tok_s=%.1f per_repeat_ms min=%.1f median=%.1f max=%.1f\n",
           rc, ntok, done, repeats, decode_seconds,
           decode_seconds > 0 ? (double)ntok * done / decode_seconds : 0.0, lo, med, hi);
    printf("DECODE RSS resident=%ld kB before=%ld kB after=%ld kB hwm(process-lifetime, includes load)=%ld kB\n",
           rss_loaded, rss_decode_before, rss_decode_after, status_kb("VmHWM:"));
    /* Sampled after the last decode returned, which is why the two configurations disagree here while their CPU
     * utilisation agrees: libgomp keeps its team alive between parallel regions, ggml's own pool is built per graph
     * plan and gone by the time this line prints. The load-bearing number is cpu_utilisation, not this count. */
    printf("DECODE CPU threads_after_last_decode=%d (before any work: %ld) cpu_utilisation=%.0f%% of one core  clk_tck=%ld\n",
           (int)threads_during, (long)threads_before, cpu_pct, (long)sysconf(_SC_CLK_TCK));

    printf("SUMMARY    variant=%s mode=%s tokens=%d repeats=%d tok_s=%.1f median_ms=%.1f spread_pct=%.2f "
           "load_s=%.2f load_read_bytes=%lld resident_after_load_kb=%ld hwm_kb=%ld threads_after=%d cpu_pct=%.0f total_s=%.2f\n",
           gomp_mapped() > 0 ? "on" : "off", cold ? "cold" : "warm", ntok, done,
           decode_seconds > 0 ? (double)ntok * done / decode_seconds : 0.0, med,
           med > 0 ? 100.0 * (hi - lo) / med : 0.0,
           load_seconds, read_after_load - read_before, rss_loaded, status_kb("VmHWM:"),
           (int)threads_during, cpu_pct, seconds_since(&t_start));

    free(per_repeat);
    free(tokens);
    free(buf);
    llama_free(ctx);
    llama_model_free(model);
    return rc == 0 ? 0 : 1;
}
