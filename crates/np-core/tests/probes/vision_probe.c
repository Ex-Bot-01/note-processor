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
 *   3. Whether `mtmd_context_params.image_min_tokens` / `image_max_tokens` — the card's 70..1120 soft-token
 *      budget — is honoured by this builder at all. Measured by re-running with different values, not by
 *      reading code.
 *   4. Comparability: cosine(caption, its image) vs cosine(caption, other image) vs cosine(caption, unrelated
 *      sentence), computed here in C so every number comes from one process and one model load.
 *
 * Build (out-of-source cmake against the vendored tree; the configure line is recorded in the note):
 *
 *   cc -O2 -o /tmp/vision-spike/vision_probe crates/np-core/tests/probes/vision_probe.c \
 *     -I .vendor/llama.cpp-v0.8.0/include -I .vendor/llama.cpp-v0.8.0/ggml/include \
 *     -I .vendor/llama.cpp-v0.8.0/tools/mtmd \
 *     /tmp/vision-spike/build/tools/mtmd/libmtmd.a /tmp/vision-spike/build/src/libllama.a \
 *     /tmp/vision-spike/build/ggml/src/libggml.a /tmp/vision-spike/build/ggml/src/libggml-cpu.a \
 *     /tmp/vision-spike/build/ggml/src/libggml-base.a -lstdc++ -lm -lpthread -ldl
 *
 * Run:
 *
 *   /tmp/vision-spike/vision_probe MODEL MMPROJ [--nctx N] [--image-min-tokens N] [--image-max-tokens N] \
 *       [--threads N] ITEM...
 *
 * Items are `img:PATH` (PNG/JPEG — decoded by mtmd's own stb_image), `txt:TEXT` (a caption) or `unrel:TEXT`
 * (an unrelated control sentence, embedded exactly like a caption so the table can name which is which). Order
 * is kept, and every vector is kept in memory, so the cosine table at the end reads in the order given. Nothing
 * is written to disk; llama.cpp's own log lines go to stderr untouched, because they are part of the evidence.
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

/* Everything the command line can say, with llama.cpp's own defaults where the probe has no opinion. */
struct opts {
    const char * model;
    const char * mmproj;
    int          n_ctx;             /* 0 = leave llama_context_default_params() alone */
    int          image_min_tokens;  /* -1 = mtmd default ("read from metadata") */
    int          image_max_tokens;  /* -1 = same */
    int          threads;           /* 0 = leave both the llama.cpp and the mtmd defaults alone */
    const char * items[MAX_ITEMS];
    int          n_items;
};

/* One result per item, kept so the cosine table can be printed once every vector exists. */
struct result {
    const char * label;  /* the ITEM argument, verbatim (minus its prefix) */
    int          kind;   /* 'i' image, 't' caption, 'u' unrelated control */
    float      * vec;    /* pooled row, n floats, malloc'd and owned here */
    size_t       n;      /* floats in vec; 0 = the item produced nothing */
};

static void usage(const char * prog) {
    fprintf(stderr,
            "usage: %s MODEL MMPROJ [--nctx N] [--image-min-tokens N] [--image-max-tokens N] [--threads N] "
            "ITEM...\n"
            "  ITEM   img:PATH | txt:TEXT | unrel:TEXT\n",
            prog);
}

/* Returns 0 on success; non-zero means exit 2 (usage already printed). */
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
            fprintf(stderr, "probe: unknown option: %s\n", a);
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
            fprintf(stderr, "probe: too many items (max %d)\n", MAX_ITEMS);
            return 1;
        }
    }
    if (positional < 2) {
        usage(argv[0]);
        return 1;
    }
    return 0;
}

/* Whole file into a malloc'd buffer, NUL-terminated for no reason beyond keeping the caller simple. */
static char * read_file(const char * path, size_t * out_len) {
    FILE * f = fopen(path, "rb");
    if (!f) {
        fprintf(stderr, "probe: cannot open %s\n", path);
        return NULL;
    }
    fseek(f, 0, SEEK_END);
    long len = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (len <= 0) {
        fclose(f);
        fprintf(stderr, "probe: %s is empty\n", path);
        return NULL;
    }
    char * buf = (char *) malloc((size_t) len + 1);
    if (!buf) {
        fclose(f);
        return NULL;
    }
    size_t got = fread(buf, 1, (size_t) len, f);
    fclose(f);
    if (got != (size_t) len) {
        free(buf);
        fprintf(stderr, "probe: short read on %s\n", path);
        return NULL;
    }
    buf[len] = '\0';
    *out_len = (size_t) len;
    return buf;
}

static float cosine(const float * a, const float * b, size_t n) {
    double dot = 0.0, na = 0.0, nb = 0.0;
    for (size_t i = 0; i < n; i++) {
        dot += (double) a[i] * b[i];
        na  += (double) a[i] * a[i];
        nb  += (double) b[i] * b[i];
    }
    if (na <= 0.0 || nb <= 0.0) {
        return 0.0f;
    }
    return (float) (dot / (sqrt(na) * sqrt(nb)));
}

static double norm(const float * v, size_t n) {
    double s = 0.0;
    for (size_t i = 0; i < n; i++) {
        s += (double) v[i] * v[i];
    }
    return sqrt(s);
}

static const char * chunk_type_name(enum mtmd_input_chunk_type t) {
    switch (t) {
        case MTMD_INPUT_CHUNK_TYPE_TEXT:  return "text";
        case MTMD_INPUT_CHUNK_TYPE_IMAGE: return "image";
        case MTMD_INPUT_CHUNK_TYPE_AUDIO: return "audio";
        default:                          return "other";
    }
}

/* The facts this build answers before a single pixel is read. */
static void print_model_facts(struct llama_model * model, struct llama_context * ctx) {
    printf("model.n_embd_inp   = %d\n", llama_model_n_embd_inp(model));
    printf("model.n_embd_out   = %d\n", llama_model_n_embd_out(model));
    printf("model.n_ctx_train  = %d\n", llama_model_n_ctx_train(model));
    printf("ctx.n_ctx          = %u\n", llama_n_ctx(ctx));
    printf("ctx.n_batch        = %u\n", llama_n_batch(ctx));
    printf("ctx.n_ubatch       = %u\n", llama_n_ubatch(ctx));
    printf("ctx.causal_attn    = %s   (the artifact's own default; restored after every image decode)\n",
           llama_get_causal_attn(ctx) ? "true" : "false");
    printf("ctx.pooling        = %d  (0=NONE 1=MEAN 2=CLS 3=RANK 4=LAST)\n", (int) llama_pooling_type(ctx));
}

static void print_mtmd_facts(struct mtmd_context * mctx) {
    printf("mtmd.support_vision      = %s\n", mtmd_support_vision(mctx) ? "true" : "false");
    printf("mtmd.support_audio       = %s\n", mtmd_support_audio(mctx) ? "true" : "false");
    printf("mtmd.default_marker      = \"%s\"\n", mtmd_default_marker());
    printf("mtmd.marker              = \"%s\"\n", mtmd_get_marker(mctx));
    printf("mtmd.decode_use_mrope    = %s\n", mtmd_decode_use_mrope(mctx) ? "true" : "false");
    printf("mtmd.use_non_causal(img) = %s\n", mtmd_decode_use_non_causal(mctx, NULL) ? "true" : "false");
}

/*
 * The pooled row of the last forward pass, copied out immediately.
 *
 * `llama_get_embeddings_seq` returns a buffer the context owns and rewrites on the next decode, so nothing here
 * keeps the pointer; the width read is `llama_model_n_embd_out`, which the product path checks against its
 * configured dimension (local_model.rs) and this probe only reports.
 */
static float * pooled_copy(struct llama_context * ctx, size_t n_out, size_t * out_n) {
    const float * pooled = llama_get_embeddings_seq(ctx, 0);
    if (!pooled) {
        fprintf(stderr, "probe: the runtime produced no pooled row\n");
        return NULL;
    }
    float * copy = (float *) malloc(n_out * sizeof(float));
    if (!copy) {
        return NULL;
    }
    memcpy(copy, pooled, n_out * sizeof(float));
    *out_n = n_out;
    return copy;
}

/*
 * Text path — the product's own sequence, with no prefix (design §1.2: instruction prefixes are text-only, and
 * captions are text). Tokenize with add_special = true / parse_special = false, one decode, read seq 0.
 */
static float * embed_text(struct llama_context * ctx, struct llama_model * model, const char * text,
                          size_t * out_n) {
    const struct llama_vocab * vocab  = llama_model_get_vocab(model);
    const int                 cap     = (int) strlen(text) + 8;
    llama_token             * tokens  = (llama_token *) malloc((size_t) cap * sizeof(llama_token));
    if (!tokens) {
        return NULL;
    }
    const int32_t n_tok = llama_tokenize(vocab, text, (int32_t) strlen(text), tokens, cap,
                                         /* add_special */ true, /* parse_special */ false);
    if (n_tok <= 0) {
        fprintf(stderr, "probe: tokenizer refused this text (rc = %d)\n", n_tok);
        free(tokens);
        return NULL;
    }
    const int32_t rc   = llama_decode(ctx, llama_batch_get_one(tokens, n_tok));
    float       * out  = NULL;
    if (rc != 0) {
        fprintf(stderr, "probe: llama_decode refused this text (rc = %d)\n", rc);
    } else {
        out = pooled_copy(ctx, (size_t) llama_model_n_embd_out(model), out_n);
    }
    free(tokens);
    return out;
}

/*
 * Image path — the sequence Tasks 4+ copy. Bytes in, one pooled row out:
 *
 *   mtmd_helper_bitmap_init_from_buf  (stb_image decode; also stamps the bitmap id with the sha256 of the bytes)
 *   mtmd_input_chunks_init
 *   mtmd_tokenize                     (prompt = the media marker alone, add_special = true, parse_special = true)
 *   mtmd_encode_chunk                 (per image chunk)
 *   mtmd_get_output_embd              (n_tokens rows of llama_model_n_embd_inp floats)
 *   llama_batch_init(n_tokens, n_embd_inp, 1) + pos/seq/logits, then llama_decode with causal attention off
 *                                     when mtmd_decode_use_non_causal says the chunk needs it
 *   llama_get_embeddings_seq(ctx, 0)  (the pooled row: n_embd_out floats)
 *
 * The text chunks around the image are tokenized but NOT decoded here: this measures one image on its own, which
 * is what design §9 asks for. `n_img_tokens` comes back so the caller can print it beside the chunk list.
 */
static float * embed_image(struct mtmd_context * mctx, struct llama_context * ctx, struct llama_model * model,
                           const char * path, size_t * out_n, size_t * out_img_tokens, size_t * out_chunks) {
    size_t        len   = 0;
    char        * buf   = read_file(path, &len);
    if (!buf) {
        return NULL;
    }

    struct mtmd_helper_init_opt opt     = mtmd_helper_init_opt_default();
    struct mtmd_helper_bitmap_wrapper w = mtmd_helper_bitmap_init_from_buf(mctx, (unsigned char *) buf, len,
                                                                          /* placeholder */ false, opt);
    free(buf);
    if (!w.bitmap) {
        fprintf(stderr, "probe: mtmd could not decode %s\n", path);
        return NULL;
    }
    /* The wrapper's second field is a video decoder; the image path leaves it null (mtmd-helper.cpp). */
    printf("  bitmap             = %ux%u id=%s\n", mtmd_bitmap_get_nx(w.bitmap), mtmd_bitmap_get_ny(w.bitmap),
           mtmd_bitmap_get_id(w.bitmap));

    const char * marker = mtmd_get_marker(mctx);
    struct mtmd_input_text txt = { marker, strlen(marker), /* add_special */ true, /* parse_special */ true };
    const mtmd_bitmap    * bitmaps[1] = { w.bitmap };

    mtmd_input_chunks * chunks = mtmd_input_chunks_init();
    const int32_t       tres   = mtmd_tokenize(mctx, chunks, &txt, bitmaps, 1);
    if (tres != 0) {
        fprintf(stderr, "probe: mtmd_tokenize refused this image (rc = %d)\n", tres);
        mtmd_input_chunks_free(chunks);
        mtmd_bitmap_free(w.bitmap);
        return NULL;
    }

    const size_t n_chunks = mtmd_input_chunks_size(chunks);
    printf("  chunks             = %zu\n", n_chunks);
    for (size_t i = 0; i < n_chunks; i++) {
        const mtmd_input_chunk * c = mtmd_input_chunks_get(chunks, i);
        printf("    chunk[%zu] %-5s n_tokens=%zu n_pos=%lld\n", i, chunk_type_name(mtmd_input_chunk_get_type(c)),
               mtmd_input_chunk_get_n_tokens(c), (long long) mtmd_input_chunk_get_n_pos(c));
    }

    const int    n_embd_inp = llama_model_n_embd_inp(model);
    const size_t n_out      = (size_t) llama_model_n_embd_out(model);
    float      * out        = NULL;
    size_t       img_tokens = 0;
    llama_seq_id seq0       = 0;

    for (size_t i = 0; i < n_chunks && !out; i++) {
        const mtmd_input_chunk * c   = mtmd_input_chunks_get(chunks, i);
        if (mtmd_input_chunk_get_type(c) == MTMD_INPUT_CHUNK_TYPE_TEXT) {
            continue;
        }
        const size_t n_tokens = mtmd_input_chunk_get_n_tokens(c);
        img_tokens           += n_tokens;

        const int32_t erc = mtmd_encode_chunk(mctx, c);
        if (erc != 0) {
            fprintf(stderr, "probe: mtmd_encode_chunk refused this image (rc = %d)\n", erc);
            break;
        }
        const float * rows = mtmd_get_output_embd(mctx);
        if (!rows) {
            fprintf(stderr, "probe: mtmd_get_output_embd returned null\n");
            break;
        }

        /*
         * Plain 1-D positions. mtmd_decode_use_mrope answers false for this projector (printed above), and
         * llama_batch.pos is section-major [n_pos_per_embd][n_tokens] when it is not — src/llama-batch.cpp:924.
         * Rather than decode an M-RoPE model with silently wrong positions, the probe refuses that case.
         */
        if (mtmd_decode_use_mrope(mctx)) {
            fprintf(stderr, "probe: M-RoPE positions are not implemented in this probe\n");
            break;
        }

        struct llama_batch b = llama_batch_init((int32_t) n_tokens, n_embd_inp, 1);
        memcpy(b.embd, rows, n_tokens * (size_t) n_embd_inp * sizeof(float));
        /*
         * Two things here that are easy to get wrong and expensive when you do:
         * `llama_batch_init` leaves **n_tokens at zero** (src/llama-batch.cpp:1093) — the caller owns the count —
         * and it **allocates one seq_id slot per token**, so writing a pointer over `seq_id[t]` makes
         * `llama_batch_free` free a stack address. Write the sequence id into the slot instead.
         */
        b.n_tokens = (int32_t) n_tokens;
        for (size_t t = 0; t < n_tokens; t++) {
            b.pos[t]       = (llama_pos) t;
            b.n_seq_id[t]  = 1;
            *b.seq_id[t]   = seq0;
            b.logits[t]    = (t + 1 == n_tokens) ? 1 : 0;
        }

        const bool non_causal = mtmd_decode_use_non_causal(mctx, c);
        const bool was_causal = llama_get_causal_attn(ctx);
        if (non_causal) {
            llama_set_causal_attn(ctx, false);
        }
        const int32_t drc = llama_decode(ctx, b);
        if (non_causal) {
            llama_set_causal_attn(ctx, was_causal);
        }
        llama_batch_free(b);

        if (drc != 0) {
            fprintf(stderr, "probe: llama_decode refused the image rows (rc = %d)\n", drc);
            break;
        }
        out = pooled_copy(ctx, n_out, out_n);
    }

    *out_img_tokens = img_tokens;
    *out_chunks     = n_chunks;
    mtmd_input_chunks_free(chunks);
    mtmd_bitmap_free(w.bitmap);
    return out;
}

/* Last path component for an image label, so four PNG paths do not print as one identical prefix. */
static const char * short_label(const struct result * r) {
    if (r->kind != 'i') {
        return r->label;
    }
    const char * slash = strrchr(r->label, '/');
    return slash ? slash + 1 : r->label;
}

static void print_table(struct result * rs, int n) {
    printf("\npairwise cosine (rows and columns in item order):\n");
    for (int i = 0; i < n; i++) {
        if (!rs[i].vec) {
            continue;
        }
        for (int j = i + 1; j < n; j++) {
            if (!rs[j].vec) {
                continue;
            }
            printf("cos[%2d %c %-34.34s][%2d %c %-34.34s] = %+0.4f\n", i, rs[i].kind, short_label(&rs[i]), j,
                   rs[j].kind, short_label(&rs[j]), cosine(rs[i].vec, rs[j].vec, rs[i].n));
        }
    }

    /* The one sentence design §9 asks for, computed rather than eyeballed. */
    printf("\ncaption score: best image cosine vs best unrelated-control cosine\n");
    for (int i = 0; i < n; i++) {
        if (rs[i].kind != 't' || !rs[i].vec) {
            continue;
        }
        float best_img = -2.0f, best_ctl = -2.0f;
        int   best_img_i = -1;
        for (int j = 0; j < n; j++) {
            if (!rs[j].vec || i == j) {
                continue;
            }
            const float c = cosine(rs[i].vec, rs[j].vec, rs[i].n);
            if (rs[j].kind == 'i' && c > best_img) {
                best_img   = c;
                best_img_i = j;
            }
            if (rs[j].kind == 'u' && c > best_ctl) {
                best_ctl = c;
            }
        }
        if (best_img_i < 0 || best_ctl <= -2.0f) {
            printf("  %-32.32s : needs at least one img: and one unrel: item\n", rs[i].label);
            continue;
        }
        printf("  %-32.32s : image %d (%+0.4f) %s control %+0.4f\n", rs[i].label, best_img_i, best_img,
               best_img > best_ctl ? "beats" : "LOSES TO", best_ctl);
    }
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
        /* Same discipline as the product path (local_model.rs open_context): n_batch and n_ubatch travel with
         * n_ctx, because a non-causal artifact asserts rather than returning when a batch is wider than its own
         * ubatch — src/llama-context.cpp:1920. */
        cparams.n_ctx    = (uint32_t) o.n_ctx;
        cparams.n_batch  = (uint32_t) o.n_ctx;
        cparams.n_ubatch = (uint32_t) o.n_ctx;
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
    mtparams.use_gpu          = false;
    mtparams.print_timings    = false;
    mtparams.warmup           = false;
    mtparams.image_min_tokens = o.image_min_tokens;
    mtparams.image_max_tokens = o.image_max_tokens;
    if (o.threads > 0) {
        mtparams.n_threads = o.threads;
    }

    printf("\n--- probe inputs ---\n");
    printf("model                = %s\n", o.model);
    printf("mmproj               = %s\n", o.mmproj);
    printf("mtmd.min_image_tokens= %d\n", mtparams.image_min_tokens);
    printf("mtmd.max_image_tokens= %d\n", mtparams.image_max_tokens);

    struct mtmd_context * mctx = mtmd_init_from_file(o.mmproj, model, mtparams);
    if (!mctx) {
        fprintf(stderr, "probe: the projector was refused by mtmd_init_from_file\n");
        llama_free(ctx);
        llama_model_free(model);
        return 1;
    }

    print_model_facts(model, ctx);
    print_mtmd_facts(mctx);

    struct result rs[MAX_ITEMS];
    memset(rs, 0, sizeof(rs));
    int     failed = 0;

    for (int i = 0; i < o.n_items; i++) {
        const char * a = o.items[i];
        if (strncmp(a, "img:", 4) == 0) {
            printf("\n--- item %d img:%s ---\n", i, a + 4);
            size_t n = 0, tok = 0, ch = 0;
            rs[i].kind  = 'i';
            rs[i].label = a + 4;
            rs[i].vec   = embed_image(mctx, ctx, model, a + 4, &n, &tok, &ch);
            rs[i].n     = n;
            if (rs[i].vec) {
                printf("  image tokens       = %zu  (chunks: %zu)\n", tok, ch);
                printf("  pooled width       = %zu floats\n", n);
                printf("  first three floats = %+0.6f %+0.6f %+0.6f\n", rs[i].vec[0], rs[i].vec[1], rs[i].vec[2]);
                printf("  pooled L2 norm     = %0.6f (before our own normalise)\n", norm(rs[i].vec, n));
            } else {
                failed = 1;
            }
        } else if (strncmp(a, "txt:", 4) == 0 || strncmp(a, "unrel:", 6) == 0) {
            const int control = a[3] == ':' ? 0 : 1;
            const char * text = a + (control ? 6 : 4);
            printf("\n--- item %d %s:%s ---\n", i, control ? "unrel" : "txt", text);
            size_t n = 0;
            rs[i].kind  = control ? 'u' : 't';
            rs[i].label = text;
            rs[i].vec   = embed_text(ctx, model, text, &n);
            rs[i].n     = n;
            if (rs[i].vec) {
                printf("  pooled width       = %zu floats\n", n);
                printf("  first three floats = %+0.6f %+0.6f %+0.6f\n", rs[i].vec[0], rs[i].vec[1], rs[i].vec[2]);
            } else {
                failed = 1;
            }
        } else {
            fprintf(stderr, "probe: item %d is neither img:, txt: nor unrel: — %s\n", i, a);
            return 2;
        }
    }

    print_table(rs, o.n_items);

    for (int i = 0; i < o.n_items; i++) {
        free(rs[i].vec);
    }
    mtmd_free(mctx);
    llama_free(ctx);
    llama_model_free(model);
    llama_backend_free();
    return failed;
}
