/*
 * The decode-side companion to `model_load_probe.c`: one forward pass through our own pinned archives, so the
 * three numbers §14 of `docs/design-m2-local-embeddings.md` quotes for a *running* model have a C answer beside
 * the Rust one. Committed for the same reason its sibling is — so that "measured" means reproducible.
 *
 * What it prints, and why each line exists:
 *
 *   MEMPROBE   `llama_get_memory(ctx)` — NULL proves this architecture has no KV cache, i.e. that
 *              `llama_context::decode` diverts to `encode` (`src/llama-context.cpp:1845-1847`) and the assert that
 *              a long sequence would hit is the encoder's at `:1631`, not the decoder's at `:1918`/`:1920`. Also
 *              prints `llama_n_threads{,_batch}`, which is how ~392% CPU happens with `GGML_OPENMP=OFF`.
 *   PROBE      token count, `llama_decode`'s return code, whether a pooled row came back, its raw L2 length (the
 *              reason R6b2 normalises: it is ≈184–220 here, never 1), and peak RSS before/after the pass.
 *
 * Build (archives from a `--features local-model` build of np-core; see `model_load_probe.c` for the same line):
 *
 *   B=$(ls -dt target/debug/build/np-core-HASH/out/llama-cpp-HASH | head -1) && \
 *   gcc -O2 -o /tmp/decode_probe crates/np-core/tests/probes/decode_probe.c \
 *     -I .vendor/llama.cpp-v0.8.0/include -I .vendor/llama.cpp-v0.8.0/ggml/include \
 *     "$B"/src/libllama.a "$B"/ggml/src/libggml.a "$B"/ggml/src/libggml-base.a "$B"/ggml/src/libggml-cpu.a \
 *     -lstdc++ -lm -lpthread -ldl
 *
 * Run:
 *
 *   /tmp/decode_probe /path/to/embeddinggemma-2-Q8_0.gguf 2048          # llama.cpp's own defaults
 *   /tmp/decode_probe /path/to/embeddinggemma-2-Q8_0.gguf 2048 wide     # n_ctx = n_batch = n_ubatch = 4096
 *   /tmp/decode_probe /path/to/embeddinggemma-2-Q8_0.gguf 512 --drop-cache   # evict our page-cache view first
 *
 * `wide` is what `local_model::load` does. Without it a sequence over 512 tokens **aborts the process**
 * (`GGML_ASSERT` → `GGML_ABORT`, live under NDEBUG) — that is the bug this probe was written to demonstrate, and
 * exit status 134 from the first form is the expected result, not a broken probe.
 */
#include "llama.h"
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static long vmhwm(void) {
    FILE * f = fopen("/proc/self/status", "r");
    if (!f) return -1;
    char line[256];
    long kb = -1;
    while (fgets(line, sizeof(line), f)) {
        if (!strncmp(line, "VmHWM:", 6)) { kb = atol(line + 6); break; }
    }
    fclose(f);
    return kb;
}

int main(int argc, char ** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s model.gguf n_tokens [wide] [--drop-cache]\n", argv[0]);
        return 2;
    }
    const char * path = argv[1];
    int want = atoi(argv[2]);
    int wide = 0, drop_cache = 0;
    for (int i = 3; i < argc; i++) {
        if (!strcmp(argv[i], "wide")) wide = 1;
        else if (!strcmp(argv[i], "--drop-cache")) drop_cache = 1;
    }

    /* Evict our own view of the artifact first (no root needed for one file's pages). The block layer may still
     * hold them, so read this as "cold from our side", the same caveat §14 attaches to the load figures. */
    if (drop_cache) {
        int fd = open(path, O_RDONLY);
        if (fd >= 0) { posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED); close(fd); }
    }

    struct llama_model_params mp = llama_model_default_params();
    struct llama_model * m = llama_model_load_from_file(path, mp);
    if (!m) { fprintf(stderr, "load refused\n"); return 1; }

    struct llama_context_params cp = llama_context_default_params();
    cp.embeddings = true;
    cp.n_ctx = 4096;
    if (wide) { cp.n_batch = 4096; cp.n_ubatch = 4096; }
    struct llama_context * c = llama_init_from_model(m, cp);
    if (!c) { fprintf(stderr, "ctx refused\n"); return 1; }

    printf("MEMPROBE: memory=%p n_threads=%d n_threads_batch=%d (wide=%d)\n",
           (void *)llama_get_memory(c), llama_n_threads(c), llama_n_threads_batch(c), wide);
    fflush(stdout);

    const struct llama_vocab * v = llama_model_get_vocab(m);
    static const char unit[] =
        "The quick brown fox jumps over the lazy dog while the rain falls softly on the roof. ";
    llama_token * tok = calloc((size_t)want + 8, sizeof(llama_token));
    int n = 0;
    while (n < want) {
        int32_t r = llama_tokenize(v, unit, (int32_t)strlen(unit), tok + n, want + 8 - n, false, false);
        if (r <= 0) { fprintf(stderr, "tokenize returned %d\n", r); break; }
        n += r;
    }
    if (n > 4090) n = 4090;

    long before = vmhwm();
    struct llama_batch b = llama_batch_get_one(tok, n);
    int rc = llama_decode(c, b);
    const float * e = llama_get_embeddings_seq(c, 0);
    double norm = 0;
    if (e) { for (int i = 0; i < 768; i++) norm += (double)e[i] * (double)e[i]; }

    printf("PROBE %s n=%d rc=%d emb=%s raw_norm=%.6f peak_rss %ld -> %ld kB\n",
           wide ? "wide" : "default", n, rc, e ? "ok" : "NULL",
           e ? __builtin_sqrt(norm) : -1.0, before, vmhwm());

    llama_free(c);
    llama_model_free(m);
    free(tok);
    return rc == 0 && e ? 0 : 1;
}
