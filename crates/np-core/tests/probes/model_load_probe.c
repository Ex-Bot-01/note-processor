/*
 * A bare C probe against the same archives build.rs links, answering one question §14 asks and a Rust test
 * alone cannot: how much of opening this model is llama.cpp, and how much is us?
 *
 * The claim it exists to test — "the Rust side costs nothing over the C probe" — needs a C number before it is
 * worth saying. Without this file that row in §14 is an unattributed peak in a document; with it the row has a
 * reproducible method: same pinned tree, same static archives, no Rust runtime in the process.
 *
 * Build against the tree build.rs already configured (replace BUILD with wherever libllama.a lives — run
 * `ls -dt target/debug/build/np-core-<hash>/out/llama-cpp-<unit>` and pick the newest):
 *
 *   B=$(ls -dt target/debug/build/np-core-HASH/out/llama-cpp-llama | head -1) && \
 *   gcc -O2 -o /tmp/model_load_probe crates/np-core/tests/probes/model_load_probe.c \
 *     -I .vendor/llama.cpp-v0.8.0/include -I .vendor/llama.cpp-v0.8.0/ggml/include \
 *     "$B"/src/libllama.a "$B"/ggml/src/libggml.a "$B"/ggml/src/libggml-cpu.a \
 *     "$B"/ggml/src/libggml-base.a -lstdc++ -lm -lpthread -ldl
 *
 * Two things about BUILD that cost a debugging session once (A2 review P2-6): the unit directory is named
 * `llama-cpp-llama` (from cmake's target name, not a hash), and when `NP_LLAMA_CPP_SRC` points at a tree that already
 * has a `CMakeCache.txt`, build.rs builds in **that tree's own `build/`** — no path under `target/debug/build` points
 * at anything then. Archive order below is dependents-before-dependencies, matching `ARCHIVES` in build.rs.
 *
 * Run:  /tmp/model_load_probe /path/to/embeddinggemma-2-Q8_0.gguf [--drop-cache] [--nctx 4096]
 *
 * `--drop-cache` calls posix_fadvise(POSIX_FADV_DONTNEED) on the artifact before loading. That is the honest way
 * to ask "what does a cold first read cost" without root: dropping the whole page cache needs root, evicting one
 * file's pages does not. It evicts this file's pages only — the block layer may still hold them, so read it as
 * "cold from our side", and say so wherever the number is quoted.
 *
 * Exit codes mirror the CLI's convention: 0 loaded, 1 refused (llama.cpp returned null), 2 bad arguments.
 */

#if defined(__linux__)
#define _GNU_SOURCE 1
#endif

#include "llama.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <fcntl.h>
#include <unistd.h>

/* One field of /proc/self/status, in kB. `field` must include its colon, because "VmRSS" is a prefix of nothing here
 * but "VmHWM:" and "VmRSS:" are distinct lines and a substring match would be luck. */
static long status_kb(const char * field) {
    FILE * f = fopen("/proc/self/status", "r");
    if (!f) {
        return -1;
    }
    const size_t skip = strlen(field);
    char         line[256];
    long         kb = -1;
    while (fgets(line, sizeof(line), f)) {
        if (strncmp(line, field, skip) == 0) {
            kb = atol(line + skip);
            break;
        }
    }
    fclose(f);
    return kb;
}

/* Peak resident set size — the high-water mark, which only ever rises. */
static long vmhwm_kb(void) {
    return status_kb("VmHWM:");
}

/* Resident set **now**. The probe used to print `vmhwm_kb()` for both its "open" and "peak" columns, so the two rightmost
 * numbers were always equal and the row that answers "what did opening this model cost" was unreadable (A2 review P1-3).
 * §14's attribution of peak RSS to llama.cpp rather than to us rests on this column being a different measurement. */
static long vmrss_kb(void) {
    return status_kb("VmRSS:");
}

static double seconds_since(double start);
static double now_seconds(void);

int main(int argc, char ** argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <model.gguf> [--drop-cache] [--nctx N]\n", argv[0]);
        return 2;
    }
    const char * path      = argv[1];
    int          drop_cache = 0;
    uint32_t     n_ctx      = 4096;

    for (int i = 2; i < argc; ++i) {
        if (strcmp(argv[i], "--drop-cache") == 0) {
            drop_cache = 1;
        } else if (strcmp(argv[i], "--nctx") == 0 && i + 1 < argc) {
            n_ctx = (uint32_t) strtoul(argv[++i], NULL, 10);
        } else {
            fprintf(stderr, "unknown argument: %s\n", argv[i]);
            return 2;
        }
    }

    if (drop_cache) {
#if defined(POSIX_FADV_DONTNEED)
        int fd = open(path, O_RDONLY);
        if (fd < 0) {
            perror("open");
            return 1;
        }
        if (posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED) != 0) {
            perror("posix_fadvise"); /* not fatal: the load below still measures what it can */
        }
        close(fd);
#else
        fprintf(stderr, "POSIX_FADV_DONTNEED unavailable on this platform; --drop-cache ignored\n");
#endif
    }

    long before = vmhwm_kb();

    struct llama_model_params mparams = llama_model_default_params();
    double                    t0      = now_seconds();
    struct llama_model *      model   = llama_model_load_from_file(path, mparams);
    if (!model) {
        fprintf(stderr, "PROBE: llama.cpp refused %s (returned null)\n", path);
        return 1;
    }
    double loaded_at = seconds_since(t0);

    struct llama_context_params cparams = llama_context_default_params();
    cparams.embeddings                  = true;
    cparams.n_ctx                       = n_ctx;
    struct llama_context * ctx          = llama_init_from_model(model, cparams);
    if (!ctx) {
        fprintf(stderr, "PROBE: llama.cpp refused to build a context (n_ctx=%u)\n", n_ctx);
        llama_model_free(model);
        return 1;
    }

    fprintf(stdout,
            "PROBE %s: load %.3f s, n_embd_out %d, n_embd_inp %d, n_ctx_train %d, vocab %d, pooling %d, "
            "RSS before %ld kB -> resident after open %ld kB (VmRSS), peak %ld kB (VmHWM)\n",
            drop_cache ? "(cold)" : "(warm)",
            loaded_at,
            llama_model_n_embd_out(model),
            llama_model_n_embd_inp(model),
            llama_model_n_ctx_train(model),
            llama_vocab_n_tokens(llama_model_get_vocab(model)),
            (int) llama_pooling_type(ctx),
            before,
            vmrss_kb(),
            vmhwm_kb());

    /* Freed in the order local_model.rs documents: context first, model second. */
    llama_free(ctx);
    llama_model_free(model);
    return 0;
}

#include <time.h>

static double now_seconds(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double) ts.tv_sec + (double) ts.tv_nsec / 1e9;
}

static double seconds_since(double start) {
    return now_seconds() - start;
}
