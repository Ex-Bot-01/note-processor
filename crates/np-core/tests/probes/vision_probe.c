/*
 * R7c vision probe — what llama.cpp v0.8.0 (this repo's pin) actually does with embeddinggemma-2's
 * `gemma4v` projector. Throwaway measurement, not product code: it exists so `docs/design-m2-image-embeddings.md`
 * §9 can be answered by the runtime instead of by reading headers, and so Tasks 4+ copy a call sequence that is
 * known to run rather than one that is merely plausible.
 *
 * Four questions, in the order the note answers them:
 *
 *   1. `mtmd_support_vision` on the pinned mmproj — recognised or not.
 *   2. Token cost of one image: chunk count and image-token count for a 1024x768 and a 512x384 PNG, against the
 *      model card's published 280 tokens per image (design §1.2).
 *   3. Whether `mtmd_context_params.image_min_tokens` / `image_max_tokens` — the soft-token budget of 70..1120 —
 *      is honoured by this builder at all. Measured by re-running with different values, not by reading code.
 *   4. Comparability: cosine(caption, its image) vs cosine(caption, other image) vs cosine(caption, unrelated
 *      sentence), computed here in C so the numbers come from one process and one model load.
 *
 * Build (out-of-source cmake against the vendored tree; see the note for the exact configure line):
 *
 *   cc -O2 -o /tmp/vision-spike/vision_probe crates/np-core/tests/probes/vision_probe.c \
 *     -I .vendor/llama.cpp-v0.8.0/include -I .vendor/llama.cpp-v0.8.0/ggml/include \
 *     -I .vendor/llama.cpp-v0.8.0/tools/mtmd \
 *     /tmp/vision-spike/build/tools/mtmd/libmtmd.a /tmp/vision-spike/build/src/libllama.a \
 *     /tmp/vision-spike/build/ggml/src/libggml.a /tmp/vision-spike/build/ggml/src/libggml-cpu.a \
 *     /tmp/vision-spike/build/ggml/src/libggml-base.a -lstdc++ -lm -lpthread -ldl
 *
 * Run (paths as arguments; the mmproj is the pinned projector, the model the pinned primary):
 *
 *   /tmp/vision-spike/vision_probe MODEL MMPROJ [--nctx N] [--image-min-tokens N] [--image-max-tokens N] \
 *       [--threads N] ITEM...
 *
 * Items are `img:PATH` (PNG/JPEG, decoded by mtmd's own stb_image) or `txt:TEXT`. Order is kept, so the cosine
 * table below reads in the order given. Nothing is written to disk.
 *
 * Exit codes follow the other probes: 0 ran, 1 refused (model, projector or one item failed), 2 bad arguments.
 */

#include "llama.h"
#include "mtmd-helper.h"
#include "mtmd.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

#define MAX_ITEMS 64

/* Everything the command line can say, with llama.cpp's own defaults where we have no opinion. */
struct opts {
    const char * model;
    const char * mmproj;
    int          n_ctx;             /* 0 = leave llama_context_default_params() alone */
    int          image_min_tokens;  /* -1 = mtmd default ("read from metadata") */
    int          image_max_tokens;  /* -1 = same */
    int          threads;           /* 0 = leave both llama.cpp and mtmd defaults alone */
    const char * items[MAX_ITEMS];
    int          n_items;
};

static void usage(const char * prog) {
    fprintf(stderr,
            "usage: %s MODEL MMPROJ [--nctx N] [--image-min-tokens N] [--image-max-tokens N] [--threads N] "
            "ITEM...\n"
            "  ITEM   img:PATH | txt:TEXT\n",
            prog);
}

/* Returns 0 on success; non-zero means the caller should exit 2 (usage already printed). */
static int parse_args(int argc, char ** argv, struct opts * o) {
    memset(o, 0, sizeof(*o));
    o->image_min_tokens = -1;
    o->image_max_tokens = -1;

    int positional = 0;
    for (int i = 1; i < argc; i++) {
        const char * a = argv[i];
        if (strcmp(a, "--nctx") == 0 && i + 1 < argc) {
            o->n_ctx = atoi(argv[++i]);
        } else if (strcmp(a, "--image-min-tokens") == 0 && i + 1 < argc) {
            o->image_min_tokens = atoi(argv[++i]);
        } else if (strcmp(a, "--image-max-tokens") == 0 && i + 1 < argc) {
            o->image_max_tokens = atoi(argv[++i]);
        } else if (strcmp(a, "--threads") == 0 && i + 1 < argc) {
            o->threads = atoi(argv[++i]);
        } else if (a[0] == '-' && a[1] != '\0') {
            fprintf(stderr, "unknown option: %s\n", a);
            usage(argv[0]);
            return 1;
        } else if (positional < 2) {
            if (positional == 0) {
                o->model = a;
            } else {
                o->mmproj = a;
            }
            positional++;
        } else if (o->n_items < MAX_ITEMS) {
            o->items[o->n_items++] = a;
        } else {
            fprintf(stderr, "too many items (max %d)\n", MAX_ITEMS);
            return 1;
        }
    }
    if (positional < 2) {
        usage(argv[0]);
        return 1;
    }
    return 0;
}

/* One result line per item, kept so the cosine table can be printed after every vector exists. */
struct result {
    const char * label;   /* the ITEM argument, verbatim */
    float      * vec;     /* pooled row, n_out floats, malloc'd */
    size_t       n;       /* floats in vec (0 = item produced nothing) */
};

/* The facts this build answers before a single pixel is read. */
static void print_model_facts(struct llama_model * model, struct llama_context * ctx) {
    printf("model.n_embd_inp   = %d\n", llama_model_n_embd_inp(model));
    printf("model.n_embd_out   = %d\n", llama_model_n_embd_out(model));
    printf("model.n_ctx_train  = %d\n", llama_model_n_ctx_train(model));
    printf("ctx.n_ctx          = %u\n", llama_n_ctx(ctx));
    printf("ctx.n_batch        = %u\n", llama_n_batch(ctx));
    printf("ctx.n_ubatch       = %u\n", llama_n_ubatch(ctx));
    printf("ctx.pooling        = %d (0=NONE 1=MEAN 2=CLS 3=RANK 4=LAST)\n", (int) llama_pooling_type(ctx));
}

static void print_mtmd_facts(struct mtmd_context * mctx) {
    printf("mtmd.support_vision      = %s\n", mtmd_support_vision(mctx) ? "true" : "false");
    printf("mtmd.support_audio       = %s\n", mtmd_support_audio(mctx) ? "true" : "false");
    printf("mtmd.default_marker      = \"%s\"\n", mtmd_default_marker());
    printf("mtmd.marker              = \"%s\"\n", mtmd_get_marker(mctx));
    printf("mtmd.decode_use_mrope    = %s\n", mtmd_decode_use_mrope(mctx) ? "true" : "false");
    printf("mtmd.decode_use_non_causal (image chunk, chunk=NULL) = %s\n",
           mtmd_decode_use_non_causal(mctx, NULL) ? "true" : "false");
}

int main(int argc, char ** argv) {
    struct opts o;
    if (parse_args(argc, argv, &o) != 0) {
        return 2;
    }

    llama_backend_init();

    struct llama_model_params mparams = llama_model_default_params();
    mparams.n_gpu_layers = 0;
    struct llama_model * model = llama_model_load_from_file(o.model, mparams);
    if (!model) {
        fprintf(stderr, "probe: the primary GGUF did not load\n");
        return 1;
    }

    struct llama_context_params cparams = llama_context_default_params();
    cparams.embeddings = true;
    if (o.n_ctx > 0) {
        /* Same discipline as the product path: n_batch and n_ubatch travel with n_ctx, because a non-causal
         * artifact aborts (rather than returning) when a batch is wider than its own ubatch. */
        cparams.n_ctx     = (uint32_t) o.n_ctx;
        cparams.n_batch   = (uint32_t) o.n_ctx;
        cparams.n_ubatch  = (uint32_t) o.n_ctx;
    }
    if (o.threads > 0) {
        cparams.n_threads       = o.threads;
        cparams.n_threads_batch = o.threads;
    }
    struct llama_context * ctx = llama_init_from_model(model, cparams);
    if (!ctx) {
        fprintf(stderr, "probe: no context for this model\n");
        llama_model_free(model);
        return 1;
    }

    struct mtmd_context_params mtparams = mtmd_context_params_default();
    mtparams.use_gpu           = false;
    mtparams.print_timings     = false;
    mtparams.warmup            = false;
    mtparams.image_min_tokens  = o.image_min_tokens;
    mtparams.image_max_tokens  = o.image_max_tokens;
    if (o.threads > 0) {
        mtparams.n_threads = o.threads;
    }
    printf("mtmd.params.image_min_tokens = %d\n", mtparams.image_min_tokens);
    printf("mtmd.params.image_max_tokens = %d\n", mtparams.image_max_tokens);

    struct mtmd_context * mctx = mtmd_init_from_file(o.mmproj, model, mtparams);
    if (!mctx) {
        fprintf(stderr, "probe: the projector was refused by mtmd_init_from_file\n");
        llama_free(ctx);
        llama_model_free(model);
        return 1;
    }

    print_model_facts(model, ctx);
    print_mtmd_facts(mctx);

    (void) o.items;
    (void) o.n_items;

    mtmd_free(mctx);
    llama_free(ctx);
    llama_model_free(model);
    llama_backend_free();
    return 0;
}
