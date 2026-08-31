/*
 * FFI crossing-cost harness (Phase-0 Task 8) — the C twin of the
 * native Rust shapes in ../../benches/ffi.rs. It includes the
 * committed corvid.h (so header drift is a COMPILE error here, the
 * same discipline as smoke.c) and drives the ABI in four shapes:
 *
 *   put     n inserts of a {i, txt, vec} map document
 *   get     n point-gets cycling the preloaded corpus
 *   scan    n full corvid_scan passes over the corpus
 *   hybrid  n vector+text RRF queries built through corvid_query_*
 *
 * Usage: bench <put|get|scan|hybrid> <iters> <corpus>
 *
 * The Rust driver times the WHOLE child and subtracts a zero-iteration
 * baseline child (setup + spawn amortize away) — no timing code in C,
 * so the file stays portable ISO C across gcc/clang/cl. The corpus is
 * pure arithmetic of the doc index (identical formulas in Rust — the
 * engine benches' seeded-determinism convention, no rand): keys are
 * "k%06u", txt is four tokens from a 50-word vocabulary, vec is 64
 * floats. Silent on success (exit 0); any wrong answer exits 1 with
 * the failing shape on stderr — a benchmark must never time garbage.
 */
#include "corvid.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define DIM 64u
#define VOCAB 50u

static void die(const char *what) {
    fprintf(stderr, "bench.c: %s failed (code %u)\n", what,
            (unsigned)corvid_last_error_code());
    exit(1);
}

static void expect_ok(corvid_status st, const char *what) {
    if (st != CORVID_OK) die(what);
}

/* key buffer for index i — "k%06u" */
static size_t key_of(unsigned i, char *buf) {
    return (size_t)sprintf(buf, "k%06u", i);
}

/* The document for index i (identical arithmetic in benches/ffi.rs):
 * {i: Int(i), txt: "w<a> w<b> w<c> w<d>", vec: [64 floats]} — an
 * owned map; callers free it after the insert (which clones, §5). */
static corvid_value *make_doc(unsigned i) {
    char txt[128];
    int n = sprintf(txt, "w%u w%u w%u w%u", (i + 0u) % VOCAB,
                    (i + 13u) % VOCAB, (i + 29u) % VOCAB, (i + 41u) % VOCAB);
    if (n < 0) {
        fprintf(stderr, "bench.c: txt formatting failed\n");
        exit(1);
    }
    float v[DIM];
    for (unsigned j = 0; j < DIM; j++) {
        unsigned t = (i * 37u + j * 11u) % 2000u;
        v[j] = (float)t / 1000.0f - 1.0f;
    }
    corvid_value *m = corvid_value_map_new();
    if (!m) die("corvid_value_map_new");
    expect_ok(corvid_value_map_put(m, "i", 1, corvid_value_int((int64_t)i)),
              "corvid_value_map_put(i)");
    expect_ok(corvid_value_map_put(m, "txt", 3, corvid_value_text(txt, (size_t)n)),
              "corvid_value_map_put(txt)");
    expect_ok(corvid_value_map_put(m, "vec", 3, corvid_value_vector(v, DIM)),
              "corvid_value_map_put(vec)");
    return m;
}

/* The hybrid query vector (same formula in Rust): q[j] from seed 7. */
static void query_vec(float *q) {
    for (unsigned j = 0; j < DIM; j++) {
        unsigned t = (7u * 37u + j * 11u) % 2000u;
        q[j] = (float)t / 1000.0f - 1.0f;
    }
}

static void setup(corvid_coll *c, unsigned corpus) {
    char key[16];
    for (unsigned i = 0; i < corpus; i++) {
        size_t klen = key_of(i, key);
        corvid_value *doc = make_doc(i);
        expect_ok(corvid_insert(c, (const uint8_t *)key, klen, doc),
                  "corvid_insert(setup)");
        corvid_value_free(doc); /* cloned into the tree (§5 rule 3) */
    }
}

static int scan_count;

static int scan_cb(void *ctx, const uint8_t *key, size_t key_len,
                   const corvid_value *doc) {
    (void)ctx;
    (void)key;
    (void)key_len;
    (void)doc;
    scan_count++;
    return 1; /* continue */
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: bench <put|get|scan|hybrid> <iters> <corpus>\n");
        return 2;
    }
    const char *mode = argv[1];
    unsigned long iters = strtoul(argv[2], NULL, 10);
    unsigned long corpus = strtoul(argv[3], NULL, 10);
    if (corpus == 0ul || corpus > 1000000ul) {
        fprintf(stderr, "bench.c: corpus out of range\n");
        return 2;
    }

    corvid_db *db = corvid_open_memory();
    if (!db) die("corvid_open_memory");
    corvid_coll *c = corvid_collection(db, "bench", 5);
    if (!c) die("corvid_collection");

    if (strcmp(mode, "put") == 0) {
        char key[16];
        for (unsigned long i = 0; i < iters; i++) {
            /* keys continue past the corpus so puts never collide */
            size_t klen = key_of((unsigned)(corpus + i), key);
            corvid_value *doc = make_doc((unsigned)(corpus + i));
            expect_ok(corvid_insert(c, (const uint8_t *)key, klen, doc),
                      "corvid_insert(put)");
            corvid_value_free(doc);
        }
    } else if (strcmp(mode, "get") == 0) {
        setup(c, (unsigned)corpus);
        char key[16];
        for (unsigned long i = 0; i < iters; i++) {
            unsigned idx = (unsigned)(i % corpus);
            size_t klen = key_of(idx, key);
            corvid_value *doc = NULL;
            expect_ok(corvid_get(c, (const uint8_t *)key, klen, &doc),
                      "corvid_get");
            if (!doc) {
                fprintf(stderr, "bench.c: get: key absent\n");
                return 1;
            }
            const corvid_value *iv = corvid_value_map_get(doc, "i", 1);
            int ok = 0;
            int64_t got = corvid_value_as_int(iv, &ok);
            if (!ok || got != (int64_t)idx) {
                fprintf(stderr, "bench.c: get: wrong field (%lld, want %u)\n",
                        (long long)got, idx);
                return 1;
            }
            corvid_value_free(doc);
        }
    } else if (strcmp(mode, "scan") == 0) {
        setup(c, (unsigned)corpus);
        for (unsigned long i = 0; i < iters; i++) {
            scan_count = 0;
            expect_ok(corvid_scan(c, scan_cb, NULL), "corvid_scan");
            if ((unsigned long)scan_count != corpus) {
                fprintf(stderr, "bench.c: scan saw %d rows, want %lu\n",
                        scan_count, corpus);
                return 1;
            }
        }
    } else if (strcmp(mode, "hybrid") == 0) {
        setup(c, (unsigned)corpus);
        float q[DIM];
        query_vec(q);
        for (unsigned long i = 0; i < iters; i++) {
            corvid_query *qb = corvid_query_new(c);
            if (!qb) die("corvid_query_new");
            expect_ok(corvid_query_vector(qb, "vec", 3, q, DIM, 10,
                                          CORVID_METRIC_COSINE),
                      "corvid_query_vector");
            expect_ok(corvid_query_text(qb, "txt", 3, "w3 w17", 6, 10),
                      "corvid_query_text");
            corvid_rows *rows = corvid_query_run(qb); /* consumes qb */
            if (!rows) die("corvid_query_run");
            unsigned long n = 0;
            const uint8_t *rk = NULL;
            size_t rk_len = 0;
            const corvid_value *rd = NULL;
            float score = 0.0f;
            while (corvid_rows_next(rows, &rk, &rk_len, &rd, &score)) n++;
            if (n == 0ul) {
                fprintf(stderr, "bench.c: hybrid: empty result\n");
                return 1;
            }
            corvid_rows_free(rows);
        }
    } else {
        fprintf(stderr, "bench.c: unknown mode %s\n", mode);
        return 2;
    }

    corvid_collection_free(c);
    corvid_close(db);
    return 0;
}
