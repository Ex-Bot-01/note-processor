/*
 * The OpenMP A/B: the same pinned llama.cpp built twice, one forward pass through each, and the difference in
 * wall-clock and memory printed rather than argued about.
 *
 * Why a separate probe when `decode_probe.c` exists: that file documents itself as *not* measuring wall time
 * (A2 review P2-7 — throughput claims belong to the Rust test), and this one's whole purpose is throughput under
 * two configurations. Leaving the old probe's contract intact keeps every §14 citation beside the code that
 * produced it.
 *
 * What it prints, one line per fact:
 *
 *   BUILD      `llama_version()` plus whether libgomp is actually mapped into this process — read out of
 *              `/proc/self/maps`, so "the ON build is the ON build" is evidence from the binary rather than a claim
 *              about a cmake flag. Version alone cannot tell the two apart: `LLAMA_BUILD_IS_DEV=ON` makes every
 *              tree between v0.8.0 and v0.8.1 report `0.8.0-dev`, which is why `third_party/llama-cpp.toml` pins a
 *              header digest instead.
 *   LOAD       wall time to load the artifact and build a context, the change in resident set, and the change in
 *              `/proc/self/io` read_bytes — the last of these is §9's open question about an mmap-first load ("does
 *              it read all 310 MB?"). A big RSS jump with small read_bytes means pages were mapped and touched
 *              without a `read(2)`; `--cold` drops our page-cache view first so the number is not flattered by a
 *              file the kernel already holds.
 *   DECODE     token count actually decoded, wall time, ms per decode, tokens/s, peak RSS. Identical token counts
 *              across runs are what make the two builds comparable, so the sequence is truncated to the requested
 *              length rather than "about" it.
 *
 * Measured with it on 2026-10-31, this host, the pinned Q8_0 artifact, alternating A/B on an idle machine — the table
 * lives in `docs/design-m2-local-embeddings.md` §14; the short form is that `GGML_OPENMP=ON` changed nothing:
 *
 *   403 tokens warm   OFF 157.7 tok/s   ON 157.4 tok/s   peak 463,788 vs 464,252 kB
 *   403 tokens cold   OFF 156.7 tok/s   ON 157.6 tok/s   peak 463,632 vs 464,000 kB
 *   7,992 tokens cold OFF  81.8 tok/s   ON  81.9 tok/s   peak 1,500,720 vs 1,501,116 kB
 *   7,992 tokens warm OFF  82.0 tok/s   ON  82.0 tok/s   peak 1,500,800 vs 1,500,940 kB
 *
 * both builds printing `n_threads=4 n_threads_batch=4`, and cold loads moving `read_bytes` by the whole 309,858,304-byte
 * file in ≈ 0.1 s. Reproduce with the two build lines below plus the two runs at the foot of this comment.
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
 * Run (the two must not overlap — this probe exists to measure a machine doing one thing):
 *
 *   /tmp/ab_off /path/to/embeddinggemma-2-Q8_0.gguf 8192 403 8 --cold
 *   /tmp/ab_on  /path/to/embeddinggemma-2-Q8_0.gguf 8192 403 8 --cold
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
#include <time.h>
#include <unistd.h>

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

/* Bytes this process has actually read from the page cache, from its own /proc entry. */
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

/* llama.cpp is chatty at info level (one line per graph tensor); a throughput probe wants its own lines to be the
 * readable part of the run, so only warnings and errors reach stderr. */
static void log_filter(enum ggml_log_level level, const char * text, void * user_data) {
    (void)user_data;
    if (level >= GGML_LOG_LEVEL_WARN) fputs(text, stderr);
}

static double seconds_since(struct timespec * start) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (double)(now.tv_sec - start->tv_sec) + (double)(now.tv_nsec - start->tv_nsec) / 1e9;
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

    llama_log_set(log_filter, NULL);
    printf("BUILD      llama_version=%s gomp_mapped=%s\n", llama_version(), gomp_mapped() ? "yes" : "no");

    if (cold) {
        int fd = open(path, O_RDONLY);
        if (fd >= 0) {
            posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
            close(fd);
        }
    }

    long rss_before = status_kb("VmRSS:");
    long long read_before = io_read_bytes();
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
    printf("LOAD       %.2f s  n_ctx_requested=%d built_n_ctx=%d n_threads=%d n_threads_batch=%d\n",
           load_seconds, nctx, llama_n_ctx(ctx), llama_n_threads(ctx), llama_n_threads_batch(ctx));
    printf("LOAD RSS   before=%ld kB after=%ld kB delta=%ld kB\n",
           rss_before, rss_loaded, rss_loaded - rss_before);
    printf("LOAD IO    read_bytes_delta=%lld bytes (%.1f MiB; the artifact is %.1f MiB)\n",
           read_after_load - read_before,
           (double)(read_after_load - read_before) / (1024.0 * 1024.0),
           (double)llama_model_size(model) / (1024.0 * 1024.0));

    /* A sequence of exactly `target` tokens: repeat a sentence, then truncate. The same token ids in both builds,
     * which is the only way a throughput difference belongs to OpenMP rather than to the input. */
    const char * unit = "The quick brown fox jumps over the lazy dog while the rain falls softly on the roof. ";
    size_t unit_len = strlen(unit);
    size_t buf_len = (size_t)(target + 4) * unit_len;
    char * buf = malloc(buf_len + 1);
    if (!buf) { fprintf(stderr, "OOM\n"); return 1; }
    for (size_t off = 0; off < buf_len; off += unit_len) memcpy(buf + off, unit, unit_len);
    buf[buf_len] = '\0';

    const struct llama_vocab * vocab = llama_model_get_vocab(model);
    /* v0.8.0's `llama_tokenize` cannot size a buffer by being passed NULL, so the upper bound is the character
     * count: a BPE never emits more tokens than it is given characters. */
    int room = (int)(buf_len + 8);
    llama_token * tokens = malloc((size_t)room * sizeof(llama_token));
    if (!tokens) { fprintf(stderr, "OOM\n"); return 1; }
    int ntok = llama_tokenize(vocab, buf, (int)buf_len, tokens, room, true, false);
    if (ntok < 0) { fprintf(stderr, "tokenize failed (%d)\n", ntok); return 1; }
    if (ntok < target) { fprintf(stderr, "only %d tokens available for a target of %d\n", ntok, target); return 1; }
    ntok = target;

    struct timespec t_decode;
    clock_gettime(CLOCK_MONOTONIC, &t_decode);
    int rc = -1;
    for (int i = 0; i < repeats; i++) {
        struct llama_batch batch = llama_batch_get_one(tokens, ntok);
        rc = llama_decode(ctx, batch);
        if (rc != 0) break;
    }
    double decode_seconds = seconds_since(&t_decode);

    printf("DECODE     rc=%d tokens=%d repeats=%d wall=%.2f s per_decode=%.1f ms tok_s=%.1f\n",
           rc, ntok, repeats, decode_seconds,
           repeats ? 1000.0 * decode_seconds / repeats : 0.0,
           decode_seconds > 0 ? (double)ntok * repeats / decode_seconds : 0.0);
    printf("DECODE RSS peak=%ld kB\n", status_kb("VmHWM:"));

    free(tokens);
    free(buf);
    llama_free(ctx);
    llama_model_free(model);
    return rc == 0 ? 0 : 1;
}
