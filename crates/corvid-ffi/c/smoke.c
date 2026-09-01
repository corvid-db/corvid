/* smoke.c — the C golden-suite harness (Phase-0 Task 7).
 *
 * Drives the typed ABI (corvid.h, docs/FFI.md) over the committed golden
 * fixtures (the golden directory): one OP<TAB>args<TAB>expected line at a time,
 * every line dispatched, every expectation checked. Compiled and run by
 * the Rust test in src/smoke.rs (the cc crate at TEST time, linked
 * against the just-built cdylib) so `cargo test --workspace` enforces
 * it; CI can also run the compiled binary standalone. The fixtures are
 * test-time inputs — the ABI itself parses nothing.
 *
 * CONVENTIONS (enforced by the C-surface radar, src/radar.rs):
 *   - NO macros that mention `corvid_` and no includes beyond system
 *     headers and "corvid.h" — macro indirection would evade the
 *     radar's call scan (it looks for the token `corvid_*` followed
 *     by `(`, after stripping comments).
 *   - Every handle/buffer this harness creates is freed on the path
 *     that created it: the ASan/LSan variant (see src/smoke.rs docs)
 *     must report ZERO leaks, exercising every handle family's free
 *     function plus corvid_free's buffer domain.
 *
 * Fixture grammar (per line): OP \t ARGS \t EXPECTED
 *   - '#' lines and blank lines are ignored (not counted as executable).
 *   - ARGS / EXPECTED are top-level comma-separated tokens; nesting
 *     inside []{}() protects its commas.
 *   - Value literals: null true false | -123 | 3.5 | inf -inf |
 *     bits:0x7ff8000000000001 (f64 from bits) | bits32:0x7fc00000 (f32)
 *     | t(text) | b(bytes) | vec(1.5,bits32:0x...,2) | [a,b] |
 *     {k=v,k2=v2}; keys/paths/relations are bare words.
 *   - Computed doubles (distances, scores, sums) expect `~x` (1e-6
 *     relative tolerance, libm-safe across platforms); stored literals
 *     compare bit-exactly (strtod is correctly rounded and the engine
 *     preserves f64 bits — NaN payloads included).
 *
 * Output protocol (parsed by src/smoke.rs):
 *   stdout: "SMOKE <file> lines=<n> executed=<n>" per fixture file.
 *   stderr + exit 1 on the first failure, naming file:line, the OP, and
 *   expected vs got.
 */

#include <math.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "corvid.h"

/* ------------------------------------------------------------------ */
/* Failure and check plumbing                                          */
/* ------------------------------------------------------------------ */

static const char *g_file = "?";
static int g_line = 0;
static const char *g_op = "?";

static void fail(const char *fmt, ...) {
    va_list ap;
    fprintf(stderr, "FAIL %s:%d OP=%s: ", g_file, g_line, g_op);
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fprintf(stderr, "\n");
    exit(1);
}

#define CHECK(cond, ...)                \
    do {                                \
        if (!(cond)) fail(__VA_ARGS__); \
    } while (0)

/* Expect a CORVID_ERR status with exactly this code (and a recorded
 * message — every error expectation drives the error-reporting pair,
 * corvid_last_error_code and corvid_last_error_message). */
static void expect_err(corvid_status st, corvid_err code) {
    if (st != CORVID_ERR) fail("expected CORVID_ERR, got CORVID_OK");
    if (corvid_last_error_code() != code)
        fail("expected error code %u, got %u", (unsigned)code,
             (unsigned)corvid_last_error_code());
    size_t msg_len = 0;
    const char *msg = corvid_last_error_message(&msg_len);
    if (msg == NULL || msg_len == 0)
        fail("error code %u recorded but the message is missing",
             (unsigned)code);
}

static void expect_ok(corvid_status st) {
    if (st != CORVID_OK)
        fail("expected CORVID_OK, got CORVID_ERR code %u",
             (unsigned)corvid_last_error_code());
}

/* ------------------------------------------------------------------ */
/* Spans and tokenizing                                                */
/* ------------------------------------------------------------------ */

typedef struct {
    const char *p;
    size_t n;
} Span;

static Span span_of(const char *s) {
    Span r;
    r.p = s;
    r.n = strlen(s);
    return r;
}

static int span_eq(Span a, Span b) {
    return a.n == b.n && (a.n == 0 || memcmp(a.p, b.p, a.n) == 0);
}

static int span_is(Span a, const char *s) { return span_eq(a, span_of(s)); }

/* Split `in` on top-level commas (depth-aware over []{}()), into `out`
 * (cap entries). Empty input yields 0 tokens. */
static int split_top(Span in, Span *out, int cap) {
    int count = 0, depth = 0;
    size_t start = 0;
    for (size_t i = 0; i <= in.n; i++) {
        char c = i < in.n ? in.p[i] : ',';
        if (c == '[' || c == '{' || c == '(') depth++;
        else if (c == ']' || c == '}' || c == ')') depth--;
        if (c == ',' && depth == 0) {
            size_t end = i;
            while (end > start && (in.p[end - 1] == ' ' || in.p[end - 1] == '\r'))
                end--;
            if (end > start) {
                if (count >= cap) fail("too many tokens (max %d)", cap);
                out[count].p = in.p + start;
                out[count].n = end - start;
                count++;
            }
            start = i + 1;
        }
    }
    return count;
}

static double parse_double(Span s) {
    char buf[64];
    if (s.n >= sizeof(buf)) fail("numeric token too long");
    memcpy(buf, s.p, s.n);
    buf[s.n] = 0;
    if (s.n > 5 && memcmp(s.p, "bits:", 5) == 0) {
        uint64_t bits = strtoull(buf + 5, NULL, 16);
        double d;
        memcpy(&d, &bits, 8);
        return d;
    }
    if (strcmp(buf, "inf") == 0) return INFINITY;
    if (strcmp(buf, "-inf") == 0) return -INFINITY;
    if (strcmp(buf, "nan") == 0) return NAN;
    return strtod(buf, NULL);
}

static uint64_t double_bits(double d) {
    uint64_t bits;
    memcpy(&bits, &d, 8);
    return bits;
}

static int double_exact(double got, double want) {
    return double_bits(got) == double_bits(want);
}

static int double_near(double got, double want) {
    double diff = fabs(got - want);
    return diff <= 1e-6 * (1.0 + fabs(want));
}

/* Match one expected-double token: `~x` near; `=x`/`x`/bits:/inf
 * bit-exact. */
static int double_matches(double got, Span tok) {
    if (tok.n > 0 && tok.p[0] == '~')
        return double_near(got, parse_double((Span){tok.p + 1, tok.n - 1}));
    if (tok.n > 0 && tok.p[0] == '=')
        return double_exact(got, parse_double((Span){tok.p + 1, tok.n - 1}));
    return double_exact(got, parse_double(tok));
}

static long long parse_i64(Span s) {
    char buf[32];
    if (s.n >= sizeof(buf)) fail("int token too long");
    memcpy(buf, s.p, s.n);
    buf[s.n] = 0;
    return strtoll(buf, NULL, 10);
}

static int parse_int(Span s) { return (int)parse_i64(s); }

/* The err:N expected token → its code. */
static corvid_err err_token(Span expected) {
    CHECK(expected.n > 4 && memcmp(expected.p, "err:", 4) == 0,
          "error expectation must be err:N, got '%.*s'", (int)expected.n,
          expected.p);
    char buf[8];
    size_t nl = expected.n - 4 < 7 ? expected.n - 4 : 7;
    memcpy(buf, expected.p + 4, nl);
    buf[nl] = 0;
    return (corvid_err)(unsigned)strtoul(buf, NULL, 10);
}

/* ------------------------------------------------------------------ */
/* Value literals: parse + build                                       */
/* ------------------------------------------------------------------ */

static void skip_ws(const char **pp, const char *end) {
    while (*pp < end && (**pp == ' ' || **pp == '\r')) (*pp)++;
}

static corvid_value *build_lit(const char **pp, const char *end);

/* Find the matching close paren for the '(' at `open`. */
static const char *match_paren(const char *open, const char *end) {
    int depth = 0;
    for (const char *q = open; q < end; q++) {
        if (*q == '(') depth++;
        else if (*q == ')') {
            depth--;
            if (depth == 0) return q;
        }
    }
    fail("unbalanced () in literal");
    return end;
}

/* Does the text at `p` start with `word` as a delimited token? */
static int starts_word(const char *p, const char *end, const char *word) {
    size_t wl = strlen(word);
    if ((size_t)(end - p) < wl || memcmp(p, word, wl) != 0) return 0;
    char after = p[wl];
    return after == ',' || after == ']' || after == '}' || after == ' ' ||
           after == '\r' || p + wl == end;
}

static corvid_value *build_number(const char **pp, const char *end) {
    const char *start = *pp;
    int is_float = 0, is_bits = 0;
    if (starts_word(*pp, end, "inf") || starts_word(*pp, end, "-inf") ||
        starts_word(*pp, end, "nan")) {
        size_t wl = starts_word(*pp, end, "-inf") ? 4 : 3;
        Span tok = {start, wl};
        *pp += wl;
        return corvid_value_float(parse_double(tok));
    }
    if ((size_t)(end - start) > 5 && memcmp(start, "bits:", 5) == 0) {
        is_float = 1;
        is_bits = 1;
        *pp = start + 5; /* scan the hex payload only */
    }
    while (*pp < end) {
        char c = **pp;
        if ((c >= '0' && c <= '9') || c == '-' || c == '+')
            (*pp)++;
        else if (c == '.' || c == 'e' || c == 'E') {
            is_float = 1;
            (*pp)++;
        } else if (is_bits && ((c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F') ||
                               c == 'x' || c == 'X')) {
            (*pp)++;
        } else
            break;
    }
    Span tok = {start, (size_t)(*pp - start)};
    if (tok.n == 0) fail("empty numeric literal");
    if (is_bits) {
        /* re-include the prefix parse_double expects */
        tok.p = start;
        tok.n = (size_t)(*pp - start);
        return corvid_value_float(parse_double(tok));
    }
    if (is_float) {
        char buf[64];
        if (tok.n >= sizeof(buf)) fail("float literal too long");
        memcpy(buf, tok.p, tok.n);
        buf[tok.n] = 0;
        return corvid_value_float(strtod(buf, NULL));
    }
    return corvid_value_int((int64_t)parse_i64(tok));
}

static corvid_value *build_lit(const char **pp, const char *end) {
    skip_ws(pp, end);
    if (*pp >= end) fail("empty literal");
    const char *start = *pp;
    char c = **pp;

    if (c == '-' || (c >= '0' && c <= '9')) return build_number(pp, end);

    /* bits:0x... (f64 from bits) starts with 'b' but is a NUMBER, not
     * the b(...) bytes literal; inf/-inf/nan likewise. */
    if (((size_t)(end - start) > 5 && memcmp(start, "bits:", 5) == 0) ||
        starts_word(start, end, "inf") || starts_word(start, end, "-inf") ||
        starts_word(start, end, "nan"))
        return build_number(pp, end);

    if (starts_word(*pp, end, "null")) {
        *pp += 4;
        return corvid_value_null();
    }
    if (starts_word(*pp, end, "true")) {
        *pp += 4;
        return corvid_value_bool(1);
    }
    if (starts_word(*pp, end, "false")) {
        *pp += 5;
        return corvid_value_bool(0);
    }

    /* t(...) / b(...) / vec(...) — balanced-paren payload. t( and b(
     * are one-char heads; vec( is a three-char head. */
    if ((c == 't' || c == 'b') && start + 1 < end && start[1] == '(') {
        const char *open = start + 1;
        const char *close = match_paren(open, end);
        Span body = {open + 1, (size_t)(close - open - 1)};
        *pp = close + 1;
        if (c == 't') return corvid_value_text(body.p, body.n);
        if (c == 'b') return corvid_value_bytes((const uint8_t *)body.p, body.n);
        {
            Span toks[32];
            int n = split_top(body, toks, 32);
            float vals[32];
            for (int i = 0; i < n; i++) {
                if (toks[i].n > 7 && memcmp(toks[i].p, "bits32:", 7) == 0) {
                    char buf[16];
                    size_t hl = toks[i].n - 7;
                    if (hl >= sizeof(buf)) fail("bits32 token too long");
                    memcpy(buf, toks[i].p + 7, hl);
                    buf[hl] = 0;
                    uint32_t bits = (uint32_t)strtoul(buf, NULL, 16);
                    float f;
                    memcpy(&f, &bits, 4);
                    vals[i] = f;
                } else {
                    vals[i] = (float)parse_double(toks[i]);
                }
            }
            return corvid_value_vector(vals, (size_t)n);
        }
    }

    if (c == 'v' && (size_t)(end - start) > 4 &&
        memcmp(start, "vec(", 4) == 0) {
        const char *open = start + 3;
        const char *close = match_paren(open, end);
        Span body = {open + 1, (size_t)(close - open - 1)};
        *pp = close + 1;
        Span toks[32];
        int n = split_top(body, toks, 32);
        float vals[32];
        for (int i = 0; i < n; i++) {
            if (toks[i].n > 7 && memcmp(toks[i].p, "bits32:", 7) == 0) {
                char buf[16];
                size_t hl = toks[i].n - 7;
                if (hl >= sizeof(buf)) fail("bits32 token too long");
                memcpy(buf, toks[i].p + 7, hl);
                buf[hl] = 0;
                uint32_t bits = (uint32_t)strtoul(buf, NULL, 16);
                float f;
                memcpy(&f, &bits, 4);
                vals[i] = f;
            } else {
                vals[i] = (float)parse_double(toks[i]);
            }
        }
        return corvid_value_vector(vals, (size_t)n);
    }

    if (c == '[') {
        int depth = 0;
        const char *close = NULL;
        for (const char *q = start; q < end; q++) {
            if (*q == '[') depth++;
            else if (*q == ']') {
                depth--;
                if (depth == 0) {
                    close = q;
                    break;
                }
            }
        }
        if (!close) fail("unbalanced [] in literal");
        corvid_value *arr = corvid_value_array_new();
        const char *q = start + 1;
        while (q < close) {
            corvid_value *item = build_lit(&q, close);
            expect_ok(corvid_value_array_push(arr, item)); /* consumes item */
            skip_ws(&q, close);
            if (q < close && *q == ',') q++;
        }
        *pp = close + 1;
        return arr;
    }

    if (c == '{') {
        int depth = 0;
        const char *close = NULL;
        for (const char *q = start; q < end; q++) {
            if (*q == '{') depth++;
            else if (*q == '}') {
                depth--;
                if (depth == 0) {
                    close = q;
                    break;
                }
            }
        }
        if (!close) fail("unbalanced {} in literal");
        corvid_value *map = corvid_value_map_new();
        const char *q = start + 1;
        skip_ws(&q, close);
        while (q < close) {
            const char *ks = q;
            while (q < close && *q != '=' && *q != ',' && *q != '}') q++;
            if (q >= close || *q != '=') fail("map literal needs k=v pairs");
            Span key = {ks, (size_t)(q - ks)};
            while (key.n > 0 && key.p[0] == ' ') {
                key.p++;
                key.n--;
            }
            q++; /* past '=' */
            corvid_value *val = build_lit(&q, close);
            expect_ok(corvid_value_map_put(map, key.p, key.n, val)); /* consumes */
            skip_ws(&q, close);
            if (q < close && *q == ',') q++;
            skip_ws(&q, close);
        }
        *pp = close + 1;
        return map;
    }

    fail("unparseable literal at '%.*s'",
         (int)((size_t)(end - start) > 24 ? 24 : (size_t)(end - start)), start);
    return NULL;
}

static corvid_value *lit(Span s) {
    const char *p = s.p;
    if (s.n == 0) fail("empty literal token");
    return build_lit(&p, s.p + s.n);
}

/* ------------------------------------------------------------------ */
/* Structural comparison through the READ API                          */
/* ------------------------------------------------------------------ */

static int values_equal(const corvid_value *got, const corvid_value *want);

static int values_equal(const corvid_value *got, const corvid_value *want) {
    if (got == NULL || want == NULL) return got == want;
    corvid_value_type_t gt = corvid_value_type(got);
    corvid_value_type_t wt = corvid_value_type(want);
    if (gt != wt) return 0;
    switch (gt) {
    case CORVID_TYPE_NULL:
        return 1;
    case CORVID_TYPE_BOOL: {
        int go = 0, wo = 0;
        int gb = corvid_value_as_bool(got, &go);
        int wb = corvid_value_as_bool(want, &wo);
        return go && wo && gb == wb;
    }
    case CORVID_TYPE_INT: {
        int go = 0, wo = 0;
        int64_t gi = corvid_value_as_int(got, &go);
        int64_t wi = corvid_value_as_int(want, &wo);
        return go && wo && gi == wi;
    }
    case CORVID_TYPE_FLOAT: {
        int go = 0, wo = 0;
        double gd = corvid_value_as_float(got, &go);
        double wd = corvid_value_as_float(want, &wo);
        return go && wo && double_exact(gd, wd);
    }
    case CORVID_TYPE_TEXT: {
        size_t gl = 0, wl = 0;
        const char *gp = corvid_value_text_ref(got, &gl);
        const char *wp = corvid_value_text_ref(want, &wl);
        return gp && wp && gl == wl && memcmp(gp, wp, gl) == 0;
    }
    case CORVID_TYPE_BYTES: {
        size_t gl = 0, wl = 0;
        const uint8_t *gp = corvid_value_bytes_ref(got, &gl);
        const uint8_t *wp = corvid_value_bytes_ref(want, &wl);
        return gp && wp && gl == wl && memcmp(gp, wp, gl) == 0;
    }
    case CORVID_TYPE_VECTOR: {
        size_t gl = 0, wl = 0;
        const float *gp = corvid_value_vector_ref(got, &gl);
        const float *wp = corvid_value_vector_ref(want, &wl);
        if (!gp || !wp || gl != wl) return 0;
        for (size_t i = 0; i < gl; i++) {
            uint32_t gb, wb;
            memcpy(&gb, &gp[i], 4);
            memcpy(&wb, &wp[i], 4);
            if (gb != wb) return 0;
        }
        return 1;
    }
    case CORVID_TYPE_ARRAY: {
        size_t gl = corvid_value_len(got), wl = corvid_value_len(want);
        if (gl != wl) return 0;
        for (size_t i = 0; i < gl; i++)
            if (!values_equal(corvid_value_array_get(got, i),
                              corvid_value_array_get(want, i)))
                return 0;
        return 1;
    }
    case CORVID_TYPE_MAP:
        return 0; /* handled by check_value via the want token's keys */
    }
    return 0;
}

/* Map comparison key-by-key: re-walk the expected literal's k=v pairs
 * (the fixture's expectation stays the key source here — equality only
 * needs the known keys; the VMAP_KEYS/GET_KEYS OPs exercise the real
 * key iterator, corvid_value_map_keys, on their own lines). */
static int maps_equal(const corvid_value *g, const corvid_value *w,
                      Span want_tok) {
    if (corvid_value_len(g) != corvid_value_len(w)) return 0;
    const char *p = want_tok.p;
    const char *end = want_tok.p + want_tok.n;
    skip_ws(&p, end);
    if (p >= end) return corvid_value_len(g) == 0; /* {} */
    p++; /* past { */
    while (p < end) {
        skip_ws(&p, end);
        const char *ks = p;
        while (p < end && *p != '=' && *p != '}') p++;
        if (p >= end || *p != '=') fail("malformed map expectation");
        Span key = {ks, (size_t)(p - ks)};
        while (key.n > 0 && key.p[0] == ' ') {
            key.p++;
            key.n--;
        }
        p++; /* past = */
        int depth = 0;
        const char *vs = p;
        while (p < end) {
            if (*p == '[' || *p == '{' || *p == '(') depth++;
            else if (*p == ']' || *p == '}' || *p == ')') {
                if (depth == 0) break;
                depth--;
            } else if (*p == ',' && depth == 0) break;
            p++;
        }
        Span vtok = {vs, (size_t)(p - vs)};
        const corvid_value *wc = corvid_value_map_get(w, key.p, key.n);
        if (wc == NULL) fail("want-side map lacks key '%.*s' — parser bug",
                             (int)key.n, key.p);
        corvid_value *wv = lit(vtok);
        int eq = values_equal(corvid_value_map_get(g, key.p, key.n), wv) ||
                 (corvid_value_type(wv) == CORVID_TYPE_MAP &&
                  maps_equal(corvid_value_map_get(g, key.p, key.n), wv, vtok));
        corvid_value_free(wv);
        if (!eq) return 0;
        skip_ws(&p, end);
        if (p >= end || *p == '}') break;
        if (*p == ',') p++;
    }
    return 1;
}

/* Render a value into buf for failure messages (best effort). */
static void render_value(const corvid_value *v, char *buf, size_t cap) {
    if (v == NULL) {
        snprintf(buf, cap, "NULL");
        return;
    }
    switch (corvid_value_type(v)) {
    case CORVID_TYPE_NULL: snprintf(buf, cap, "null"); break;
    case CORVID_TYPE_BOOL: {
        int ok = 0;
        snprintf(buf, cap, "bool(%d)", corvid_value_as_bool(v, &ok));
        break;
    }
    case CORVID_TYPE_INT: {
        int ok = 0;
        snprintf(buf, cap, "int(%lld)",
                 (long long)corvid_value_as_int(v, &ok));
        break;
    }
    case CORVID_TYPE_FLOAT: {
        int ok = 0;
        double d = corvid_value_as_float(v, &ok);
        snprintf(buf, cap, "float(0x%016llx=%g)",
                 (unsigned long long)double_bits(d), d);
        break;
    }
    case CORVID_TYPE_TEXT: {
        size_t l = 0;
        const char *p = corvid_value_text_ref(v, &l);
        snprintf(buf, cap, "text(%.*s)", (int)(l > 40 ? 40 : l), p);
        break;
    }
    case CORVID_TYPE_BYTES: {
        size_t l = 0;
        const uint8_t *p = corvid_value_bytes_ref(v, &l);
        snprintf(buf, cap, "bytes(%.*s)", (int)(l > 40 ? 40 : l),
                 (const char *)p);
        break;
    }
    case CORVID_TYPE_VECTOR: {
        size_t l = 0, used;
        const float *p = corvid_value_vector_ref(v, &l);
        used = (size_t)snprintf(buf, cap, "vec(dim=%zu", l);
        for (size_t i = 0; i < l && i < 6; i++)
            used += (size_t)snprintf(buf + used, cap - used, ",%g", p[i]);
        snprintf(buf + used, cap - used, ")");
        break;
    }
    case CORVID_TYPE_ARRAY:
        snprintf(buf, cap, "array(len=%zu)", corvid_value_len(v));
        break;
    case CORVID_TYPE_MAP:
        snprintf(buf, cap, "map(len=%zu)", corvid_value_len(v));
        break;
    }
}

/* Compare a got value (owned or borrowed) against an expected literal
 * token; the want value is built, compared, freed. */
static void check_value(const corvid_value *got, Span want_tok) {
    corvid_value *want = lit(want_tok);
    char gbuf[160], wbuf[160];
    render_value(got, gbuf, sizeof gbuf);
    render_value(want, wbuf, sizeof wbuf);
    int eq = values_equal(got, want) ||
             (corvid_value_type(want) == CORVID_TYPE_MAP &&
              got != NULL && maps_equal(got, want, want_tok));
    corvid_value_free(want);
    CHECK(eq, "value mismatch: got %s, want %s", gbuf, wbuf);
}

/* ------------------------------------------------------------------ */
/* Scenario state                                                      */
/* ------------------------------------------------------------------ */

typedef struct {
    corvid_db *db;
    corvid_coll *coll;
    char workdir[512];
    char db_path[512], db2_path[512], dump_path[512], backup_path[512];
    long long last_auto_id; /* INSERT_AUTO monotonicity */
} Scenario;

static Scenario S;

static void close_coll(void) {
    if (S.coll) {
        corvid_collection_free(S.coll);
        S.coll = NULL;
    }
}

static void close_db(void) {
    close_coll();
    if (S.db) {
        expect_ok(corvid_close(S.db));
        S.db = NULL;
    }
}

/* (Re)acquire the primary "docs" collection handle. */
static corvid_coll *docs(void) {
    if (!S.coll) {
        CHECK(S.db != NULL, "no database open in this scenario");
        S.coll = corvid_collection(S.db, "docs", 4);
        CHECK(S.coll != NULL, "corvid_collection(docs) failed");
    }
    return S.coll;
}

static void open_memory(void) {
    close_db();
    S.db = corvid_open_memory();
    CHECK(S.db != NULL, "corvid_open_memory failed");
    (void)docs();
}

static void open_file(const char *path) {
    close_db();
    S.db = corvid_open(path, strlen(path));
    CHECK(S.db != NULL, "corvid_open(%s) failed", path);
    (void)docs();
}

/* ------------------------------------------------------------------ */
/* Callbacks (§1.6)                                                    */
/* ------------------------------------------------------------------ */

typedef struct {
    int count;
    int stop_after;
} ScanCtx;

static int scan_sink(void *ctx, const uint8_t *key, size_t key_len,
                     const corvid_value *doc) {
    (void)key;
    (void)key_len;
    (void)doc;
    ScanCtx *c = (ScanCtx *)ctx;
    c->count++;
    if (c->stop_after > 0 && c->count >= c->stop_after) return 0;
    return 1;
}

static corvid_status update_bump(void *ctx, const corvid_value *current,
                                 corvid_value **out) {
    (void)ctx;
    long long n = 0;
    if (current != NULL) {
        const corvid_value *f = corvid_value_map_get(current, "n", 1);
        CHECK(f != NULL, "update_bump: current doc lacks field n");
        int ok = 0;
        n = corvid_value_as_int(f, &ok);
        CHECK(ok, "update_bump: field n is not an int");
    }
    corvid_value *map = corvid_value_map_new();
    expect_ok(corvid_value_map_put(map, "n", 1, corvid_value_int(n + 1)));
    *out = map;
    return CORVID_OK;
}

static corvid_status update_abort(void *ctx, const corvid_value *current,
                                  corvid_value **out) {
    (void)ctx;
    (void)current;
    *out = NULL;
    return CORVID_ERR; /* the aborting-callback contract (§1.6) */
}

/* ------------------------------------------------------------------ */
/* Cursor walkers                                                      */
/* ------------------------------------------------------------------ */

#define MAX_ROWS 64

typedef struct {
    Span keys[MAX_ROWS];
    char key_buf[MAX_ROWS][128];
    float scores[MAX_ROWS];
    int n;
} RowWalk;

static void walk_rows(corvid_rows *rows, RowWalk *w) {
    w->n = 0;
    for (;;) {
        const uint8_t *key = NULL;
        size_t key_len = 0;
        const corvid_value *doc = NULL;
        float score = 0.0f;
        if (corvid_rows_next(rows, &key, &key_len, &doc, &score) != 1) break;
        CHECK(w->n < MAX_ROWS, "more rows than the walker holds");
        CHECK(key_len < sizeof w->key_buf[0], "row key too long");
        memcpy(w->key_buf[w->n], key, key_len);
        w->key_buf[w->n][key_len] = 0;
        w->keys[w->n] = (Span){w->key_buf[w->n], key_len};
        w->scores[w->n] = score;
        w->n++;
    }
}

/* Match "k(a,b,c)" — key order exact. */
static void check_keys(const RowWalk *w, Span expected) {
    CHECK(expected.n >= 3 && expected.p[0] == 'k' && expected.p[1] == '(' &&
              expected.p[expected.n - 1] == ')',
          "key expectation must be k(...)");
    Span body = {expected.p + 2, expected.n - 3};
    Span want[32];
    int nw = body.n == 0 ? 0 : split_top(body, want, 32);
    CHECK(w->n == nw, "row count %d, expected %d", w->n, nw);
    for (int i = 0; i < nw; i++)
        CHECK(span_eq(w->keys[i], want[i]),
              "row %d key '%.*s', expected '%.*s'", i, (int)w->keys[i].n,
              w->keys[i].p, (int)want[i].n, want[i].p);
}

/* Match a "|~s1,~s2" suffix — one double token per row. */
static void check_scores(const RowWalk *w, Span suffix) {
    if (suffix.n == 0) return;
    CHECK(suffix.p[0] == '|', "score suffix must start with |");
    Span body = {suffix.p + 1, suffix.n - 1};
    if (body.n == 0) return;
    Span toks[32];
    int nw = split_top(body, toks, 32);
    CHECK(w->n == nw, "score count %d, expected %d", w->n, nw);
    for (int i = 0; i < nw; i++) {
        double got = (double)w->scores[i];
        CHECK(double_matches(got, toks[i]),
              "row %d score %.9g does not match '%.*s'", i, got,
              (int)toks[i].n, toks[i].p);
    }
}

static Span key_part(Span expected) {
    for (size_t i = 0; i < expected.n; i++)
        if (expected.p[i] == '|') return (Span){expected.p, i};
    return expected;
}

static Span suffix_part(Span expected) {
    for (size_t i = 0; i < expected.n; i++)
        if (expected.p[i] == '|') return (Span){expected.p + i, expected.n - i};
    return (Span){expected.p, 0};
}

/* ------------------------------------------------------------------ */
/* Predicate helpers                                                   */
/* ------------------------------------------------------------------ */

static corvid_cmp parse_cmp(Span s) {
    if (span_is(s, "eq")) return CORVID_CMP_EQ;
    if (span_is(s, "ne")) return CORVID_CMP_NE;
    if (span_is(s, "lt")) return CORVID_CMP_LT;
    if (span_is(s, "le")) return CORVID_CMP_LE;
    if (span_is(s, "gt")) return CORVID_CMP_GT;
    if (span_is(s, "ge")) return CORVID_CMP_GE;
    fail("bad cmp op '%.*s'", (int)s.n, s.p);
    return CORVID_CMP_EQ;
}

static corvid_metric parse_metric(Span s) {
    if (span_is(s, "cosine")) return CORVID_METRIC_COSINE;
    if (span_is(s, "dot")) return CORVID_METRIC_DOT;
    if (span_is(s, "l2")) return CORVID_METRIC_L2;
    fail("bad metric '%.*s'", (int)s.n, s.p);
    return CORVID_METRIC_COSINE;
}

static corvid_quant parse_quant(Span s) {
    if (span_is(s, "none")) return CORVID_QUANT_NONE;
    if (span_is(s, "binary")) return CORVID_QUANT_BINARY;
    if (span_is(s, "scalar")) return CORVID_QUANT_SCALAR;
    fail("bad quant '%.*s'", (int)s.n, s.p);
    return CORVID_QUANT_NONE;
}

static corvid_field_type parse_field_type(Span s) {
    if (span_is(s, "any")) return CORVID_FIELD_ANY;
    if (span_is(s, "bool")) return CORVID_FIELD_BOOL;
    if (span_is(s, "int")) return CORVID_FIELD_INT;
    if (span_is(s, "float")) return CORVID_FIELD_FLOAT;
    if (span_is(s, "text")) return CORVID_FIELD_TEXT;
    if (span_is(s, "bytes")) return CORVID_FIELD_BYTES;
    if (span_is(s, "vector")) return CORVID_FIELD_VECTOR;
    if (span_is(s, "array")) return CORVID_FIELD_ARRAY;
    if (span_is(s, "map")) return CORVID_FIELD_MAP;
    fail("bad field type '%.*s'", (int)s.n, s.p);
    return CORVID_FIELD_ANY;
}

/* Build a compare pred from (path, op, literal) tokens. */
static corvid_pred *cmp_pred(Span path, Span op, Span val_lit) {
    corvid_value *v = lit(val_lit);
    corvid_pred *p = corvid_pred_compare(path.p, path.n, parse_cmp(op), v);
    corvid_value_free(v); /* CLONED into the tree (§5 rule 3) */
    CHECK(p != NULL, "corvid_pred_compare failed");
    return p;
}

/* The (filter) → count workhorse: builds, filters, counts, all consumed. */
static long long filtered_count(corvid_pred *p) {
    corvid_query *q = corvid_query_new(docs());
    CHECK(q != NULL, "corvid_query_new failed");
    expect_ok(corvid_query_filter(q, p)); /* consumes p */
    size_t n = 0;
    expect_ok(corvid_query_count(q, &n)); /* consumes q */
    return (long long)n;
}

/* ------------------------------------------------------------------ */
/* Misc small helpers                                                  */
/* ------------------------------------------------------------------ */

static void expect_num(Span expected, long long got) {
    CHECK(parse_i64(expected) == got, "expected %lld, want '%.*s'", got,
          (int)expected.n, expected.p);
}

/* Walk a child path like a.b.0.c from a value; NULL when absent. */
static const corvid_value *walk_path(const corvid_value *root, Span path) {
    const corvid_value *cur = root;
    const char *p = path.p, *end = path.p + path.n;
    while (p < end && cur) {
        if (*p == '.') p++;
        const char *ks = p;
        while (p < end && *p != '.') p++;
        Span seg = {ks, (size_t)(p - ks)};
        if (seg.n == 0) break;
        char buf[64];
        if (seg.n >= sizeof buf) fail("path segment too long");
        memcpy(buf, seg.p, seg.n);
        buf[seg.n] = 0;
        int is_index = 1;
        for (size_t i = 0; i < seg.n; i++)
            if (buf[i] < '0' || buf[i] > '9') is_index = 0;
        cur = is_index
                  ? corvid_value_array_get(cur, (size_t)parse_i64(seg))
                  : corvid_value_map_get(cur, buf, seg.n);
    }
    return cur;
}

/* The t(...) literal body. */
static Span text_body(Span tok) {
    CHECK(tok.n >= 3 && tok.p[0] == 't' && tok.p[1] == '(' &&
              tok.p[tok.n - 1] == ')',
          "expected a t(...) literal, got '%.*s'", (int)tok.n, tok.p);
    return (Span){tok.p + 2, tok.n - 3};
}

/* The k(...) list body. */
static Span list_body(Span tok) {
    CHECK(tok.n >= 3 && tok.p[0] == 'k' && tok.p[1] == '(' &&
              tok.p[tok.n - 1] == ')',
          "expected a k(...) list, got '%.*s'", (int)tok.n, tok.p);
    return (Span){tok.p + 2, tok.n - 3};
}

/* ------------------------------------------------------------------ */
/* OP implementations                                                  */
/* ------------------------------------------------------------------ */

static void run_line(Span op, Span args, Span expected) {
    Span a[16];
    int na = args.n ? split_top(args, a, 16) : 0;
    (void)na;

    /* ---- pure value ops (no db) ---- */
    if (span_is(op, "VERSION")) {
        CHECK(corvid_ffi_version() == 1, "FFI_VERSION must be 1");
        return;
    }
    if (span_is(op, "VTYPE")) {
        static const char *names[] = {"null",  "bool", "int",   "float",
                                      "text",  "bytes", "array", "map",
                                      "vector"};
        corvid_value *v = lit(a[0]);
        unsigned t = (unsigned)corvid_value_type(v);
        CHECK(t <= 8, "type tag %u out of range", t);
        CHECK(span_is(expected, names[t]), "type %s, want '%.*s'", names[t],
              (int)expected.n, expected.p);
        corvid_value_free(v);
        return;
    }
    if (span_is(op, "VLEN")) {
        corvid_value *v = lit(a[0]);
        expect_num(expected, (long long)corvid_value_len(v));
        corvid_value_free(v);
        return;
    }
    if (span_is(op, "VAS_INT") || span_is(op, "VAS_FLOAT") ||
        span_is(op, "VAS_BOOL")) {
        corvid_value *v = lit(a[0]);
        int ok = 0;
        char gotbuf[160];
        if (op.p[4] == 'I') {
            long long got = corvid_value_as_int(v, &ok);
            if (span_is(expected, "fail"))
                CHECK(!ok, "as_int unexpectedly ok (%lld)", got);
            else {
                CHECK(ok, "as_int failed");
                snprintf(gotbuf, sizeof gotbuf, "ok:%lld", got);
                CHECK(span_is(expected, gotbuf), "as_int %s, want '%.*s'",
                      gotbuf, (int)expected.n, expected.p);
            }
        } else if (op.p[4] == 'F') {
            double got = corvid_value_as_float(v, &ok);
            if (span_is(expected, "fail"))
                CHECK(!ok, "as_float unexpectedly ok");
            else {
                CHECK(ok, "as_float failed");
                CHECK(expected.n > 3 && memcmp(expected.p, "ok:", 3) == 0,
                      "as_float expectation must be ok:<double>");
                Span w = {expected.p + 3, expected.n - 3};
                CHECK(double_matches(got, w),
                      "as_float 0x%016llx (%g) does not match '%.*s'",
                      (unsigned long long)double_bits(got), got, (int)w.n,
                      w.p);
            }
        } else {
            int got = corvid_value_as_bool(v, &ok);
            if (span_is(expected, "fail"))
                CHECK(!ok, "as_bool unexpectedly ok");
            else {
                CHECK(ok, "as_bool failed");
                snprintf(gotbuf, sizeof gotbuf, "ok:%d", got);
                CHECK(span_is(expected, gotbuf), "as_bool %s, want '%.*s'",
                      gotbuf, (int)expected.n, expected.p);
            }
        }
        corvid_value_free(v);
        return;
    }
    if (span_is(op, "VTEXT_REF") || span_is(op, "VBYTES_REF") ||
        span_is(op, "VVECTOR_REF")) {
        corvid_value *v = lit(a[0]);
        if (op.p[1] == 'T') {
            size_t len = 0;
            const char *p = corvid_value_text_ref(v, &len);
            Span body = text_body(expected);
            CHECK(p != NULL, "text_ref returned NULL for a text value");
            CHECK(len == body.n && memcmp(p, body.p, len) == 0,
                  "text bytes differ");
        } else if (op.p[1] == 'B') {
            size_t len = 0;
            const uint8_t *p = corvid_value_bytes_ref(v, &len);
            CHECK(expected.n >= 3 && expected.p[0] == 'b' && expected.p[1] == '(',
                  "bytes expectation must be b(...)");
            Span body = {expected.p + 2, expected.n - 3};
            CHECK(p != NULL, "bytes_ref returned NULL for a bytes value");
            CHECK(len == body.n && memcmp(p, body.p, len) == 0,
                  "bytes differ");
        } else {
            size_t dim = 0;
            const float *p = corvid_value_vector_ref(v, &dim);
            corvid_value *w = lit(a[0]);
            size_t wdim = 0;
            const float *wp = corvid_value_vector_ref(w, &wdim);
            CHECK(p != NULL, "vector_ref returned NULL for a vector value");
            CHECK(dim == wdim, "ref dim %zu, rebuilt dim %zu", dim, wdim);
            for (size_t i = 0; i < dim; i++) {
                uint32_t gb, wb;
                memcpy(&gb, &p[i], 4);
                memcpy(&wb, &wp[i], 4);
                CHECK(gb == wb, "vector elem %zu differs bit-exactly", i);
            }
            corvid_value_free(w);
        }
        corvid_value_free(v);
        return;
    }
    if (span_is(op, "VNEST") || span_is(op, "VCLONE")) {
        corvid_value *root = lit(a[0]);
        corvid_value *holder =
            span_is(op, "VCLONE") ? corvid_value_clone(root) : root;
        CHECK(holder != NULL, "clone failed");
        const corvid_value *child = walk_path(holder, a[1]);
        if (span_is(expected, "absent"))
            CHECK(child == NULL, "path unexpectedly present");
        else
            check_value(child, expected);
        if (holder != root) corvid_value_free(holder);
        corvid_value_free(root);
        return;
    }
    if (span_is(op, "VPUSH")) {
        corvid_value *arr = lit(a[0]);
        corvid_value *item = lit(a[1]);
        expect_ok(corvid_value_array_push(arr, item)); /* consumes item */
        expect_num(expected, (long long)corvid_value_len(arr));
        corvid_value_free(arr);
        return;
    }
    if (span_is(op, "VPUT")) {
        corvid_value *map = lit(a[0]);
        corvid_value *val = lit(a[2]);
        expect_ok(corvid_value_map_put(map, a[1].p, a[1].n, val)); /* consumes */
        expect_num(expected, (long long)corvid_value_len(map));
        corvid_value_free(map);
        return;
    }
    if (span_is(op, "VMAP_KEYS") || span_is(op, "GET_KEYS")) {
        /* VMAP_KEYS enumerates a literal's keys (pure form, values.txt);
         * GET_KEYS fetches an inserted document by key first, proving the
         * decode-from-storage half bindings need. Both drive the §4.12
         * strs cursor over corvid_value_map_keys' ascending byte order. */
        corvid_value *v = NULL;
        if (op.p[0] == 'G') {
            expect_ok(corvid_get(docs(), (const uint8_t *)a[0].p, a[0].n, &v));
            CHECK(v != NULL, "GET_KEYS on an absent document");
        } else {
            v = lit(a[0]);
        }
        corvid_strs *s = corvid_value_map_keys(v);
        CHECK(s != NULL, "corvid_value_map_keys failed");
        RowWalk w;
        w.n = 0;
        for (;;) {
            const char *item = NULL;
            size_t ilen = 0;
            if (corvid_strs_next(s, &item, &ilen) != 1) break;
            CHECK(w.n < MAX_ROWS, "map_keys overflow");
            CHECK(ilen < sizeof w.key_buf[0], "map key too long");
            memcpy(w.key_buf[w.n], item, ilen);
            w.key_buf[w.n][ilen] = 0;
            w.keys[w.n] = (Span){w.key_buf[w.n], ilen};
            w.n++;
        }
        corvid_strs_free(s);
        corvid_value_free(v);
        check_keys(&w, expected);
        return;
    }
    if (span_is(op, "NULLFREES")) {
        corvid_value_free(NULL);
        corvid_pred_free(NULL);
        corvid_query_free(NULL);
        corvid_rows_free(NULL);
        corvid_strs_free(NULL);
        corvid_geohits_free(NULL);
        corvid_groupiter_free(NULL);
        corvid_schemaiter_free(NULL);
        corvid_collection_free(NULL);
        corvid_free(NULL);
        return;
    }

    /* ---- db-required ops from here on ---- */
    if (span_is(op, "COLL")) {
        close_coll();
        S.coll = corvid_collection(S.db, a[0].p, a[0].n);
        CHECK(S.coll != NULL, "corvid_collection failed");
        size_t len = 0;
        const char *name = corvid_collection_name(S.coll, &len);
        CHECK(name != NULL && len == a[0].n && memcmp(name, a[0].p, len) == 0,
              "collection_name round trip failed");
        return;
    }
    if (span_is(op, "INSERT") || span_is(op, "INSERT_ERR")) {
        corvid_value *v = lit(a[1]);
        corvid_status st =
            corvid_insert(docs(), (const uint8_t *)a[0].p, a[0].n, v);
        corvid_value_free(v); /* CLONED into the engine */
        if (op.n > 6) /* INSERT_ERR */
            expect_err(st, err_token(expected));
        else
            expect_ok(st);
        return;
    }
    if (span_is(op, "LEN")) {
        size_t n = 0;
        expect_ok(corvid_len(docs(), &n));
        expect_num(expected, (long long)n);
        return;
    }
    if (span_is(op, "GET") || span_is(op, "GETFIELD")) {
        corvid_value *out = NULL;
        expect_ok(corvid_get(docs(), (const uint8_t *)a[0].p, a[0].n, &out));
        if (op.n > 3) { /* GETFIELD */
            CHECK(out != NULL, "GETFIELD on an absent document");
            const corvid_value *child = walk_path(out, a[1]);
            if (span_is(expected, "absent"))
                CHECK(child == NULL, "field unexpectedly present");
            else
                check_value(child, expected);
        } else if (span_is(expected, "absent")) {
            CHECK(out == NULL, "expected absence, got a document");
        } else {
            CHECK(out != NULL, "expected a document, got absence");
            check_value(out, expected);
        }
        corvid_value_free(out);
        return;
    }
    if (span_is(op, "PUTMANY") || span_is(op, "PUTMANY_ROLLBACK")) {
        CHECK(na % 2 == 0, "PUTMANY wants key/literal pairs");
        int count = na / 2;
        corvid_kv items[8];
        corvid_value *vals[8];
        CHECK(count <= 8, "PUTMANY cap is 8 pairs");
        for (int i = 0; i < count; i++) {
            vals[i] = lit(a[2 * i + 1]);
            items[i].key = (const uint8_t *)a[2 * i].p;
            items[i].key_len = a[2 * i].n;
            items[i].val = vals[i];
        }
        corvid_status st = corvid_put_many(docs(), items, (size_t)count);
        for (int i = 0; i < count; i++) corvid_value_free(vals[i]); /* cloned */
        if (op.n > 7) /* PUTMANY_ROLLBACK */
            expect_err(st, err_token(expected));
        else
            expect_ok(st);
        return;
    }
    if (span_is(op, "INSERT_AUTO")) {
        corvid_value *v = lit(a[0]);
        size_t klen = 0;
        uint8_t *key = corvid_insert_auto(docs(), v, &klen);
        corvid_value_free(v);
        CHECK(key != NULL, "insert_auto failed");
        CHECK(klen == 20, "auto key length %zu, want 20", klen);
        long long id = 0;
        for (size_t i = 0; i < klen; i++) {
            CHECK(key[i] >= '0' && key[i] <= '9',
                  "auto key not zero-padded digits");
            id = id * 10 + (key[i] - '0');
        }
        CHECK(S.last_auto_id == 0 || id > S.last_auto_id,
              "auto id %lld not monotonic (previous %lld)", id,
              S.last_auto_id);
        S.last_auto_id = id;
        corvid_free(key);
        return;
    }
    if (span_is(op, "UPDATE")) {
        expect_ok(corvid_update(docs(), (const uint8_t *)a[0].p, a[0].n,
                                update_bump, NULL));
        return;
    }
    if (span_is(op, "UPDATE_ABORT")) {
        corvid_status st = corvid_update(docs(), (const uint8_t *)a[0].p,
                                         a[0].n, update_abort, NULL);
        expect_err(st, CORVID_E_ARGUMENT);
        return;
    }
    if (span_is(op, "PATCH")) {
        corvid_value *v = lit(a[1]);
        corvid_status st =
            corvid_patch(docs(), (const uint8_t *)a[0].p, a[0].n, v);
        corvid_value_free(v);
        expect_ok(st);
        return;
    }
    if (span_is(op, "CAS")) {
        corvid_value *ex = span_is(a[1], "absent") ? NULL : lit(a[1]);
        corvid_value *re = span_is(a[2], "absent") ? NULL : lit(a[2]);
        int32_t applied = -1;
        expect_ok(corvid_compare_and_set(docs(), (const uint8_t *)a[0].p,
                                         a[0].n, ex, re, &applied));
        if (ex) corvid_value_free(ex);
        if (re) corvid_value_free(re);
        CHECK(span_is(expected, applied ? "applied:1" : "applied:0"),
              "CAS applied=%d, want '%.*s'", applied, (int)expected.n,
              expected.p);
        return;
    }
    if (span_is(op, "DELETE")) {
        int32_t existed = -1;
        expect_ok(corvid_delete(docs(), (const uint8_t *)a[0].p, a[0].n,
                                &existed));
        CHECK(span_is(expected, existed ? "existed:1" : "existed:0"),
              "delete existed=%d, want '%.*s'", existed, (int)expected.n,
              expected.p);
        return;
    }
    if (span_is(op, "DELETE_WHERE")) {
        size_t removed = 0;
        expect_ok(corvid_delete_where(docs(), cmp_pred(a[0], a[1], a[2]),
                                      &removed)); /* consumes the pred */
        char buf[32];
        snprintf(buf, sizeof buf, "removed:%lld", (long long)removed);
        CHECK(span_is(expected, buf), "removed %s, want '%.*s'", buf,
              (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "DELETE_IN")) {
        corvid_value *vals[8];
        const corvid_value *ptrs[8];
        int n = na - 1;
        CHECK(n <= 8, "DELETE_IN cap is 8 values");
        for (int i = 0; i < n; i++) {
            vals[i] = lit(a[i + 1]);
            ptrs[i] = vals[i];
        }
        corvid_pred *p = corvid_pred_in(a[0].p, a[0].n, ptrs, (size_t)n);
        for (int i = 0; i < n; i++) corvid_value_free(vals[i]); /* cloned */
        CHECK(p != NULL, "pred_in failed");
        size_t removed = 0;
        expect_ok(corvid_delete_where(docs(), p, &removed)); /* consumes */
        char buf[32];
        snprintf(buf, sizeof buf, "removed:%lld", (long long)removed);
        CHECK(span_is(expected, buf), "removed %s, want '%.*s'", buf,
              (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "DELETE_BATCH")) {
        const uint8_t *keys[16];
        size_t lens[16];
        CHECK(na <= 16, "DELETE_BATCH cap is 16 keys");
        for (int i = 0; i < na; i++) {
            keys[i] = (const uint8_t *)a[i].p;
            lens[i] = a[i].n;
        }
        size_t removed = 0;
        expect_ok(
            corvid_delete_batch(docs(), keys, lens, (size_t)na, &removed));
        char buf[32];
        snprintf(buf, sizeof buf, "removed:%lld", (long long)removed);
        CHECK(span_is(expected, buf), "removed %s, want '%.*s'", buf,
              (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "INSERT_TTL")) {
        corvid_value *v = lit(a[1]);
        corvid_status st = corvid_insert_with_ttl(
            docs(), (const uint8_t *)a[0].p, a[0].n, v, (int64_t)parse_i64(a[2]));
        corvid_value_free(v);
        expect_ok(st);
        return;
    }
    if (span_is(op, "GET_TTL")) {
        int64_t exp = 0;
        int32_t has = -1;
        expect_ok(corvid_get_ttl(docs(), (const uint8_t *)a[0].p, a[0].n, &exp,
                                 &has));
        char buf[40];
        if (has)
            snprintf(buf, sizeof buf, "ttl:%lld", (long long)exp);
        else
            snprintf(buf, sizeof buf, "nottl");
        CHECK(span_is(expected, buf), "ttl %s, want '%.*s'", buf,
              (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "SET_TTL")) {
        expect_ok(corvid_set_ttl(docs(), (const uint8_t *)a[0].p, a[0].n,
                                 (int64_t)parse_i64(a[1])));
        return;
    }
    if (span_is(op, "PURGE")) {
        size_t purged = 0;
        expect_ok(corvid_purge_expired(docs(), (int64_t)parse_i64(a[0]),
                                       &purged));
        char buf[32];
        snprintf(buf, sizeof buf, "purged:%lld", (long long)purged);
        CHECK(span_is(expected, buf), "purged %s, want '%.*s'", buf,
              (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "SCAN") || span_is(op, "SCAN_STOP")) {
        ScanCtx ctx = {0, op.n > 4 ? parse_int(a[0]) : 0};
        expect_ok(corvid_scan(docs(), scan_sink, &ctx));
        expect_num(expected, ctx.count);
        return;
    }
    if (span_is(op, "PAGE")) {
        int from_start = span_is(a[0], "-");
        const uint8_t *after = from_start ? NULL : (const uint8_t *)a[0].p;
        size_t after_len = from_start ? 0 : a[0].n;
        corvid_rows *rows = NULL;
        uint8_t *next = NULL;
        size_t next_len = 0;
        expect_ok(corvid_page(docs(), after, after_len,
                              (size_t)parse_i64(a[1]), &rows, &next,
                              &next_len));
        RowWalk w;
        walk_rows(rows, &w);
        corvid_rows_free(rows);
        int at_end = (next == NULL);
        if (next) corvid_free(next);
        check_keys(&w, key_part(expected));
        Span sp = suffix_part(expected);
        CHECK(at_end ? span_is(sp, "|end") : span_is(sp, "|more"),
              "page cursor %s, want '%.*s'", at_end ? "end" : "more",
              (int)sp.n, sp.p);
        return;
    }

    /* ---- predicates + queries ---- */
    if (span_is(op, "QF_COUNT")) {
        expect_num(expected, filtered_count(cmp_pred(a[0], a[1], a[2])));
        return;
    }
    if (span_is(op, "QF_EXISTS")) {
        corvid_pred *p = corvid_pred_exists(a[0].p, a[0].n);
        CHECK(p != NULL, "pred_exists failed");
        expect_num(expected, filtered_count(p));
        return;
    }
    if (span_is(op, "QF_BETWEEN")) {
        corvid_value *lo = lit(a[1]), *hi = lit(a[2]);
        corvid_pred *p = corvid_pred_between(a[0].p, a[0].n, lo, hi);
        corvid_value_free(lo);
        corvid_value_free(hi);
        CHECK(p != NULL, "pred_between failed");
        expect_num(expected, filtered_count(p));
        return;
    }
    if (span_is(op, "QF_STARTS") || span_is(op, "QF_CONTAINS")) {
        Span body = text_body(a[1]);
        corvid_pred *p =
            op.p[3] == 'S'
                ? corvid_pred_starts_with(a[0].p, a[0].n, body.p, body.n)
                : corvid_pred_contains(a[0].p, a[0].n, body.p, body.n);
        CHECK(p != NULL, "text pred failed");
        expect_num(expected, filtered_count(p));
        return;
    }
    if (span_is(op, "QF_GEO")) {
        corvid_pred *p = corvid_pred_geo_within(a[0].p, a[0].n,
                                                parse_double(a[1]),
                                                parse_double(a[2]),
                                                parse_double(a[3]));
        CHECK(p != NULL, "pred_geo_within failed");
        expect_num(expected, filtered_count(p));
        return;
    }
    if (span_is(op, "QF_AND") || span_is(op, "QF_OR")) {
        corvid_pred *l = cmp_pred(a[0], a[1], a[2]);
        corvid_pred *r = cmp_pred(a[3], a[4], a[5]);
        corvid_pred *p =
            op.p[3] == 'A' ? corvid_pred_and(l, r) : corvid_pred_or(l, r);
        CHECK(p != NULL, "combinator failed (children consumed)");
        expect_num(expected, filtered_count(p));
        return;
    }
    if (span_is(op, "QF_NOT")) {
        corvid_pred *inner = cmp_pred(a[0], a[1], a[2]);
        corvid_pred *p = corvid_pred_not(inner); /* consumes */
        CHECK(p != NULL, "pred_not failed");
        expect_num(expected, filtered_count(p));
        return;
    }
    if (span_is(op, "PRED_FREE")) {
        corvid_pred *p = cmp_pred(a[0], a[1], a[2]);
        corvid_pred_free(p); /* the never-consumed-root free path */
        return;
    }
    if (span_is(op, "Q_ABANDON")) {
        corvid_query *q = corvid_query_new(docs());
        CHECK(q != NULL, "query_new failed");
        corvid_query_free(q); /* the abandoned-builder free path */
        return;
    }
    if (span_is(op, "QVEC") || span_is(op, "APPROX")) {
        corvid_query *q = corvid_query_new(docs());
        CHECK(q != NULL, "query_new failed");
        corvid_value *qv = lit(a[1]); /* vec(...) */
        size_t dim = 0;
        const float *elems = corvid_value_vector_ref(qv, &dim);
        if (op.p[0] == 'A') expect_ok(corvid_query_approx(q));
        expect_ok(corvid_query_vector(q, a[0].p, a[0].n, elems, dim,
                                      (size_t)parse_i64(a[2]),
                                      CORVID_METRIC_COSINE));
        corvid_value_free(qv);
        corvid_rows *rows = corvid_query_run(q); /* consumes q */
        CHECK(rows != NULL, "query_run failed");
        RowWalk w;
        walk_rows(rows, &w);
        corvid_rows_free(rows);
        check_keys(&w, key_part(expected));
        check_scores(&w, suffix_part(expected));
        return;
    }
    if (span_is(op, "QTEXT")) {
        corvid_query *q = corvid_query_new(docs());
        CHECK(q != NULL, "query_new failed");
        Span body = text_body(a[1]);
        expect_ok(corvid_query_text(q, a[0].p, a[0].n, body.p, body.n,
                                    (size_t)parse_i64(a[2])));
        corvid_rows *rows = corvid_query_run(q);
        CHECK(rows != NULL, "query_run failed");
        RowWalk w;
        walk_rows(rows, &w);
        corvid_rows_free(rows);
        check_keys(&w, expected);
        return;
    }
    if (span_is(op, "PHRASE") || span_is(op, "PHRASE_K0")) {
        /* The direct positional search (spec §4.6's erratum): args are
         * field, t(phrase), k — expected is k(keys) plus an optional
         * |~score suffix (the BM25 phrase sum, the rows cursor's other
         * score scale). PHRASE_K0 is the inert k==0 shape: an EMPTY
         * cursor, never NULL (the nothing-recorded half of k == 0's
         * inertness is pinned by the query.rs unit test on a fresh
         * thread — the smoke thread's last-error slot may hold an
         * earlier line's intentional failure, by §3's contract that
         * successful calls never clear it). */
        Span body = text_body(a[1]);
        corvid_rows *rows = corvid_phrase_search(
            docs(), a[0].p, a[0].n, body.p, body.n, (size_t)parse_i64(a[2]));
        CHECK(rows != NULL, "corvid_phrase_search failed");
        RowWalk w;
        walk_rows(rows, &w);
        corvid_rows_free(rows);
        check_keys(&w, key_part(expected));
        check_scores(&w, suffix_part(expected));
        if (op.n > 6) { /* PHRASE_K0: the empty-cursor half */
            CHECK(w.n == 0, "k == 0 must answer an empty cursor");
        }
        return;
    }
    if (span_is(op, "HYBRID") || span_is(op, "HYBRID_F")) {
        /* args: vfield vec k tfield t(query) tk [tagvalue] limit — the
         * tagvalue (HYBRID_F) slides the limit to the LAST slot.
         * (HYBRID adds a kind=doc filter; HYBRID_F a tag=<arg6> filter) */
        int tagged = op.n > 6;
        int vk = parse_int(a[2]);
        int tk = parse_int(a[5]);
        int limit = parse_int(tagged ? a[7] : a[6]);
        corvid_query *q = corvid_query_new(docs());
        CHECK(q != NULL, "query_new failed");
        if (tagged) {
            corvid_value *tv = lit(a[6]);
            corvid_pred *tag = corvid_pred_compare("tag", 3, CORVID_CMP_EQ, tv);
            corvid_value_free(tv);
            CHECK(tag != NULL, "tag filter build failed");
            expect_ok(corvid_query_filter(q, tag)); /* consumes */
        } else {
            corvid_value *kind = corvid_value_text("doc", 3);
            corvid_pred *p =
                corvid_pred_compare("kind", 4, CORVID_CMP_EQ, kind);
            corvid_value_free(kind);
            CHECK(p != NULL, "kind filter build failed");
            expect_ok(corvid_query_filter(q, p)); /* consumes */
        }
        corvid_value *qv = lit(a[1]);
        size_t dim = 0;
        const float *elems = corvid_value_vector_ref(qv, &dim);
        expect_ok(corvid_query_vector(q, a[0].p, a[0].n, elems, dim,
                                      (size_t)vk, CORVID_METRIC_COSINE));
        corvid_value_free(qv);
        Span body = text_body(a[4]);
        expect_ok(corvid_query_text(q, a[3].p, a[3].n, body.p, body.n,
                                    (size_t)tk));
        expect_ok(corvid_query_fuse_rrf(q, 60.0f));
        expect_ok(corvid_query_rerank_mmr(q, 1.0f));
        expect_ok(corvid_query_limit(q, (size_t)limit));
        corvid_rows *rows = corvid_query_run(q); /* consumes q */
        CHECK(rows != NULL, "query_run failed");
        RowWalk w;
        walk_rows(rows, &w);
        corvid_rows_free(rows);
        check_keys(&w, key_part(expected));
        check_scores(&w, suffix_part(expected));
        return;
    }
    if (span_is(op, "ORDER_BY")) {
        corvid_query *q = corvid_query_new(docs());
        CHECK(q != NULL, "query_new failed");
        expect_ok(
            corvid_query_order_by(q, a[0].p, a[0].n, parse_int(a[1])));
        expect_ok(corvid_query_offset(q, (size_t)parse_i64(a[2])));
        expect_ok(corvid_query_limit(q, (size_t)parse_i64(a[3])));
        corvid_rows *rows = corvid_query_run(q);
        CHECK(rows != NULL, "query_run failed");
        RowWalk w;
        walk_rows(rows, &w);
        corvid_rows_free(rows);
        check_keys(&w, expected);
        return;
    }
    if (span_is(op, "SELECT")) {
        /* args: (field,field,...) k(row-key); expected: that row's
         * projected document. The paren group keeps the field list one
         * token at the args level; strip it before splitting. */
        Span fields_arg = a[0];
        CHECK(fields_arg.n >= 2 && fields_arg.p[0] == '(' &&
                  fields_arg.p[fields_arg.n - 1] == ')',
              "SELECT's first arg must be a (field,...) group");
        fields_arg.p += 1;
        fields_arg.n -= 2;
        Span toks[8];
        int nf = split_top(fields_arg, toks, 8);
        const char *fields[8];
        size_t flens[8];
        for (int i = 0; i < nf; i++) {
            fields[i] = toks[i].p;
            flens[i] = toks[i].n;
        }
        Span want_key = list_body(a[1]);
        /* Two identical runs: the first finds the row's position, the
         * second stops on it and checks the projected document. */
        int found = -1;
        for (int pass = 0; pass < 2; pass++) {
            corvid_query *q = corvid_query_new(docs());
            CHECK(q != NULL, "query_new failed");
            expect_ok(corvid_query_select(q, fields, flens, (size_t)nf));
            corvid_rows *rows = corvid_query_run(q);
            CHECK(rows != NULL, "query_run failed");
            if (pass == 0) {
                RowWalk w;
                walk_rows(rows, &w);
                for (int i = 0; i < w.n; i++)
                    if (span_eq(w.keys[i], want_key)) found = i;
                CHECK(found >= 0, "row '%.*s' not in the result",
                      (int)want_key.n, want_key.p);
            } else {
                const corvid_value *doc = NULL;
                for (int i = 0; i <= found; i++) {
                    const uint8_t *k = NULL;
                    size_t kl = 0;
                    float sc = 0;
                    if (corvid_rows_next(rows, &k, &kl, &doc, &sc) != 1)
                        fail("rows disappeared on the second pass");
                }
                check_value(doc, expected);
            }
            corvid_rows_free(rows);
        }
        return;
    }
    if (span_is(op, "AGG_COUNT")) {
        corvid_query *q = corvid_query_new(docs());
        size_t n = 0;
        expect_ok(corvid_query_count(q, &n)); /* consumes */
        expect_num(expected, (long long)n);
        return;
    }
    if (span_is(op, "AGG_DISTINCT")) {
        corvid_query *q = corvid_query_new(docs());
        size_t n = 0;
        expect_ok(corvid_query_count_distinct(q, a[0].p, a[0].n, &n));
        expect_num(expected, (long long)n);
        return;
    }
    if (span_is(op, "AGG_SUM")) {
        corvid_query *q = corvid_query_new(docs());
        double sum = 0;
        expect_ok(corvid_query_sum(q, a[0].p, a[0].n, &sum));
        CHECK(double_matches(sum, expected), "sum %.17g vs '%.*s'", sum,
              (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "AGG_AVG")) {
        corvid_query *q = corvid_query_new(docs());
        double avg = 0;
        int has = -1;
        expect_ok(corvid_query_avg(q, a[0].p, a[0].n, &avg, &has));
        CHECK(span_is(expected, "none") ? !has : has, "avg has=%d, want '%.*s'",
              has, (int)expected.n, expected.p);
        if (has)
            CHECK(double_matches(avg, expected), "avg %.17g vs '%.*s'", avg,
                  (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "AGG_MIN") || span_is(op, "AGG_MAX")) {
        corvid_query *q = corvid_query_new(docs());
        corvid_value *out = NULL;
        corvid_status st = op.p[5] == 'I'
                               ? corvid_query_min(q, a[0].p, a[0].n, &out)
                               : corvid_query_max(q, a[0].p, a[0].n, &out);
        expect_ok(st);
        if (span_is(expected, "absent"))
            CHECK(out == NULL, "expected absence");
        else {
            CHECK(out != NULL, "expected a value");
            check_value(out, expected);
            corvid_value_free(out);
        }
        return;
    }
    if (span_is(op, "AGG_GCOUNT") || span_is(op, "AGG_GSUM") ||
        span_is(op, "AGG_GAVG")) {
        corvid_query *q = corvid_query_new(docs());
        corvid_groupiter *it = NULL;
        if (op.p[5] == 'C')
            it = corvid_query_group_count(q, a[0].p, a[0].n);
        else if (op.p[5] == 'S')
            it = corvid_query_group_sum(q, a[0].p, a[0].n, a[1].p, a[1].n);
        else
            it = corvid_query_group_avg(q, a[0].p, a[0].n, a[1].p, a[1].n);
        CHECK(it != NULL, "group aggregate failed (query consumed)");
        /* §7 inert rule exercised once with a NULL handle. */
        CHECK(corvid_groupiter_next(NULL, NULL, NULL, NULL) == 0,
              "NULL-handle groupiter_next must answer 0");
        CHECK(expected.n >= 3 && expected.p[0] == 'g' && expected.p[1] == '(' &&
                  expected.p[expected.n - 1] == ')',
              "group expectation must be g(...)");
        Span body = {expected.p + 2, expected.n - 3};
        Span pairs[32];
        int np = body.n == 0 ? 0 : split_top(body, pairs, 32);
        for (int i = 0; i < np; i++) {
            const char *key = NULL;
            size_t key_len = 0;
            double val = 0;
            CHECK(corvid_groupiter_next(it, &key, &key_len, &val) == 1,
                  "group %d of %d missing", i + 1, np);
            size_t eq = (size_t)-1;
            for (size_t c = 0; c < pairs[i].n; c++)
                if (pairs[i].p[c] == '=') eq = c;
            CHECK(eq != (size_t)-1, "group pair needs key=val");
            CHECK(key_len == eq && memcmp(key, pairs[i].p, key_len) == 0,
                  "group key '%.*s', want '%.*s'", (int)key_len, key,
                  (int)eq, pairs[i].p);
            Span vtok = {pairs[i].p + eq + 1, pairs[i].n - eq - 1};
            CHECK(double_matches(val, vtok),
                  "group '%.*s' value %.17g vs '%.*s'", (int)key_len, key,
                  val, (int)vtok.n, vtok.p);
        }
        {
            const char *k = NULL;
            size_t kl = 0;
            double v = 0;
            CHECK(corvid_groupiter_next(it, &k, &kl, &v) == 0,
                  "group cursor not exhausted after %d pairs", np);
        }
        corvid_groupiter_free(it);
        return;
    }

    /* ---- graph ---- */
    if (span_is(op, "LINK")) {
        expect_ok(corvid_link(docs(), (const uint8_t *)a[0].p, a[0].n, a[1].p,
                              a[1].n, (const uint8_t *)a[2].p, a[2].n));
        return;
    }
    if (span_is(op, "LINK_W")) {
        expect_ok(corvid_link_weighted(docs(), (const uint8_t *)a[0].p, a[0].n,
                                       a[1].p, a[1].n,
                                       (const uint8_t *)a[2].p, a[2].n,
                                       parse_double(a[3])));
        return;
    }
    if (span_is(op, "UNLINK")) {
        int32_t removed = -1;
        expect_ok(corvid_unlink(docs(), (const uint8_t *)a[0].p, a[0].n,
                                a[1].p, a[1].n, (const uint8_t *)a[2].p,
                                a[2].n, &removed));
        CHECK(span_is(expected, removed ? "removed:1" : "removed:0"),
              "unlink removed=%d, want '%.*s'", removed, (int)expected.n,
              expected.p);
        return;
    }
    if (span_is(op, "NEIGHBORS") || span_is(op, "IN_NEIGHBORS")) {
        corvid_strs *s =
            op.p[0] == 'N'
                ? corvid_neighbors(docs(), (const uint8_t *)a[0].p, a[0].n,
                                   a[1].p, a[1].n)
                : corvid_in_neighbors(docs(), (const uint8_t *)a[0].p, a[0].n,
                                      a[1].p, a[1].n);
        CHECK(s != NULL, "neighbors failed");
        RowWalk w;
        w.n = 0;
        for (;;) {
            const char *item = NULL;
            size_t ilen = 0;
            if (corvid_strs_next(s, &item, &ilen) != 1) break;
            CHECK(w.n < MAX_ROWS, "neighbors overflow");
            CHECK(ilen < sizeof w.key_buf[0], "neighbor key too long");
            memcpy(w.key_buf[w.n], item, ilen);
            w.key_buf[w.n][ilen] = 0;
            w.keys[w.n] = (Span){w.key_buf[w.n], ilen};
            w.n++;
        }
        corvid_strs_free(s);
        check_keys(&w, expected);
        return;
    }
    if (span_is(op, "NEIGHBORS_W")) {
        corvid_geohits *h = corvid_neighbors_weighted(
            docs(), (const uint8_t *)a[0].p, a[0].n, a[1].p, a[1].n);
        CHECK(h != NULL, "neighbors_weighted failed");
        CHECK(expected.n >= 3 && expected.p[0] == 'g' && expected.p[1] == '(' &&
                  expected.p[expected.n - 1] == ')',
              "weighted expectation must be g(...)");
        Span body = {expected.p + 2, expected.n - 3};
        Span pairs[32];
        int np = body.n == 0 ? 0 : split_top(body, pairs, 32);
        int i = 0;
        for (;;) {
            corvid_geohit hit = {NULL, 0, 0};
            const corvid_value *doc = (const corvid_value *)1;
            if (corvid_geohits_next(h, &hit, &doc) != 1) break;
            CHECK(doc == NULL, "weighted hits carry no document (§4.12)");
            CHECK(i < np, "more weighted hits (%d) than expected (%d)", i + 1,
                  np);
            size_t eq = (size_t)-1;
            for (size_t c = 0; c < pairs[i].n; c++)
                if (pairs[i].p[c] == '=') eq = c;
            CHECK(eq != (size_t)-1, "weighted pair needs key=val");
            CHECK(hit.key_len == eq && memcmp(hit.key, pairs[i].p, eq) == 0,
                  "weighted key '%.*s', want '%.*s'", (int)hit.key_len,
                  hit.key, (int)eq, pairs[i].p);
            Span vtok = {pairs[i].p + eq + 1, pairs[i].n - eq - 1};
            CHECK(double_matches(hit.distance_km, vtok),
                  "weight of '%.*s' %.17g vs '%.*s'", (int)hit.key_len,
                  hit.key, hit.distance_km, (int)vtok.n, vtok.p);
            i++;
        }
        CHECK(i == np, "weighted hits %d, expected %d", i, np);
        corvid_geohits_free(h);
        return;
    }
    if (span_is(op, "TRAVERSE")) {
        corvid_strs *s = corvid_traverse(docs(), (const uint8_t *)a[0].p, a[0].n,
                                         a[1].p, a[1].n,
                                         (size_t)parse_i64(a[2]));
        CHECK(s != NULL, "traverse failed");
        RowWalk w;
        w.n = 0;
        for (;;) {
            const char *item = NULL;
            size_t ilen = 0;
            if (corvid_strs_next(s, &item, &ilen) != 1) break;
            CHECK(w.n < MAX_ROWS, "traverse overflow");
            CHECK(ilen < sizeof w.key_buf[0], "traverse key too long");
            memcpy(w.key_buf[w.n], item, ilen);
            w.key_buf[w.n][ilen] = 0;
            w.keys[w.n] = (Span){w.key_buf[w.n], ilen};
            w.n++;
        }
        corvid_strs_free(s);
        check_keys(&w, expected);
        return;
    }

    /* ---- geo ---- */
    if (span_is(op, "GINSERT") || span_is(op, "GINSERT_M")) {
        corvid_value *map = corvid_value_map_new();
        corvid_value *loc;
        if (op.n > 7) { /* GINSERT_M: {lat, lon} map form */
            loc = corvid_value_map_new();
            expect_ok(corvid_value_map_put(
                loc, "lat", 3, corvid_value_float(parse_double(a[1]))));
            expect_ok(corvid_value_map_put(
                loc, "lon", 3, corvid_value_float(parse_double(a[2]))));
        } else {
            loc = corvid_value_array_new();
            expect_ok(corvid_value_array_push(
                loc, corvid_value_float(parse_double(a[1]))));
            expect_ok(corvid_value_array_push(
                loc, corvid_value_float(parse_double(a[2]))));
        }
        expect_ok(corvid_value_map_put(map, "loc", 3, loc));
        corvid_status st =
            corvid_insert(docs(), (const uint8_t *)a[0].p, a[0].n, map);
        corvid_value_free(map);
        expect_ok(st);
        return;
    }
    if (span_is(op, "RADIUS") || span_is(op, "NEAREST") || span_is(op, "BBOX")) {
        corvid_geohits *h = NULL;
        if (op.p[0] == 'R')
            h = corvid_geo_within_radius(docs(), a[0].p, a[0].n,
                                         parse_double(a[1]), parse_double(a[2]),
                                         parse_double(a[3]));
        else if (op.p[0] == 'N')
            h = corvid_geo_nearest(docs(), a[0].p, a[0].n, parse_double(a[1]),
                                   parse_double(a[2]), (size_t)parse_i64(a[3]));
        else
            h = corvid_geo_within_bbox(docs(), a[0].p, a[0].n,
                                       parse_double(a[1]), parse_double(a[2]),
                                       parse_double(a[3]), parse_double(a[4]));
        CHECK(h != NULL, "geo query failed");
        RowWalk w;
        w.n = 0;
        double dists[MAX_ROWS];
        int nd = 0;
        for (;;) {
            corvid_geohit hit = {NULL, 0, 0};
            const corvid_value *doc = NULL;
            if (corvid_geohits_next(h, &hit, &doc) != 1) break;
            CHECK(doc != NULL, "geo hits carry their document");
            CHECK(w.n < MAX_ROWS, "geo overflow");
            CHECK(hit.key_len < sizeof w.key_buf[0], "geo key too long");
            memcpy(w.key_buf[w.n], hit.key, hit.key_len);
            w.key_buf[w.n][hit.key_len] = 0;
            w.keys[w.n] = (Span){w.key_buf[w.n], hit.key_len};
            w.n++;
            if (nd < MAX_ROWS) dists[nd++] = hit.distance_km;
        }
        corvid_geohits_free(h);
        check_keys(&w, key_part(expected));
        Span sp = suffix_part(expected);
        if (sp.n) {
            CHECK(sp.p[0] == '|', "geo suffix must start with |");
            Span body = {sp.p + 1, sp.n - 1};
            Span toks[32];
            int nt = body.n ? split_top(body, toks, 32) : 0;
            CHECK(nd == nt, "distance count %d, expected %d", nd, nt);
            for (int i = 0; i < nt; i++)
                CHECK(double_matches(dists[i], toks[i]),
                      "hit %d distance %.9g vs '%.*s'", i, dists[i],
                      (int)toks[i].n, toks[i].p);
        }
        return;
    }
    if (span_is(op, "BBOX_ERR")) {
        corvid_geohits *h = corvid_geo_within_bbox(
            docs(), a[0].p, a[0].n, parse_double(a[1]), parse_double(a[2]),
            parse_double(a[3]), parse_double(a[4]));
        CHECK(h == NULL, "bbox unexpectedly succeeded");
        (void)h;
        expect_err(CORVID_ERR, err_token(expected));
        return;
    }

    /* ---- schema & indexes ---- */
    if (span_is(op, "SET_SCHEMA")) {
        corvid_field_def defs[16];
        Span flds[16];
        int n = split_top(args, flds, 16);
        for (int i = 0; i < n; i++) {
            /* field specs split on '#' (no nesting inside a spec) */
            Span part[4];
            int np = 0;
            const char *fp = flds[i].p, *fe = flds[i].p + flds[i].n;
            const char *seg = fp;
            while (fp <= fe && np < 4) {
                if (fp == fe || *fp == '#') {
                    part[np].p = seg;
                    part[np].n = (size_t)(fp - seg);
                    np++;
                    seg = fp + 1;
                }
                fp++;
            }
            CHECK(np == 4, "field spec needs name#type#required#unique");
            defs[i].name = part[0].p;
            defs[i].name_len = part[0].n;
            defs[i].type = parse_field_type(part[1]);
            defs[i].required = span_is(part[2], "1");
            defs[i].unique = span_is(part[3], "1");
        }
        expect_ok(corvid_set_schema(docs(), defs, (size_t)n));
        return;
    }
    if (span_is(op, "SCHEMA")) {
        static const char *tn[] = {"any", "bool", "int", "float", "text",
                                   "bytes", "array", "map", "vector"};
        corvid_schemaiter *it = NULL;
        expect_ok(corvid_schema(docs(), &it));
        CHECK(it != NULL, "a schema must be declared first");
        char got[512];
        size_t used = 0;
        for (;;) {
            corvid_field_def f = {NULL, 0, CORVID_FIELD_ANY, 0, 0};
            if (corvid_schemaiter_next(it, &f) != 1) break;
            if (used)
                used += (size_t)snprintf(got + used, sizeof got - used, ",");
            used += (size_t)snprintf(got + used, sizeof got - used,
                                     "%.*s/%s/%d/%d", (int)f.name_len, f.name,
                                     tn[f.type % 9], f.required, f.unique);
        }
        corvid_schemaiter_free(it);
        CHECK(span_is(expected, got), "schema %s, want '%.*s'", got,
              (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "SCHEMA9")) {
        static const char *names[9] = {"f_any", "f_bool", "f_int", "f_float",
                                       "f_text", "f_bytes", "f_vector",
                                       "f_array", "f_map"};
        corvid_field_type types[9] = {
            CORVID_FIELD_ANY,    CORVID_FIELD_BOOL,  CORVID_FIELD_INT,
            CORVID_FIELD_FLOAT,  CORVID_FIELD_TEXT,  CORVID_FIELD_BYTES,
            CORVID_FIELD_VECTOR, CORVID_FIELD_ARRAY, CORVID_FIELD_MAP};
        corvid_field_def defs[9];
        for (int i = 0; i < 9; i++) {
            defs[i].name = names[i];
            defs[i].name_len = strlen(names[i]);
            defs[i].type = types[i];
            defs[i].required = i == 1;
            defs[i].unique = i == 8;
        }
        expect_ok(corvid_set_schema(docs(), defs, 9));
        corvid_schemaiter *it = NULL;
        expect_ok(corvid_schema(docs(), &it));
        CHECK(it != NULL, "the 9-field schema must be declared");
        char got[64];
        size_t used = 0;
        int i = 0;
        for (;;) {
            corvid_field_def f = {NULL, 0, CORVID_FIELD_ANY, 0, 0};
            if (corvid_schemaiter_next(it, &f) != 1) break;
            CHECK(i < 9 && f.type == types[i] &&
                      f.name_len == strlen(names[i]) &&
                      memcmp(f.name, names[i], f.name_len) == 0,
                  "field %d did not round-trip", i);
            if (i)
                used += (size_t)snprintf(got + used, sizeof got - used, ",");
            used += (size_t)snprintf(got + used, sizeof got - used, "%u",
                                     (unsigned)f.type);
            i++;
        }
        corvid_schemaiter_free(it);
        CHECK(i == 9, "expected exactly 9 fields, saw %d", i);
        CHECK(span_is(expected, got), "schema9 %s, want '%.*s'", got,
              (int)expected.n, expected.p);
        return;
    }
    if (span_is(op, "SCHEMA_ERR")) {
        corvid_value *v = lit(a[1]);
        corvid_status st =
            corvid_insert(docs(), (const uint8_t *)a[0].p, a[0].n, v);
        corvid_value_free(v);
        expect_err(st, err_token(expected));
        return;
    }
    if (span_is(op, "IDX_SCALAR")) {
        expect_ok(corvid_create_scalar_index(docs(), a[0].p, a[0].n));
        return;
    }
    if (span_is(op, "IDX_COMPOUND")) {
        Span flds[8];
        int n = split_top(args, flds, 8);
        const char *names[8];
        size_t lens[8];
        for (int i = 0; i < n; i++) {
            names[i] = flds[i].p;
            lens[i] = flds[i].n;
        }
        expect_ok(
            corvid_create_compound_index(docs(), names, lens, (size_t)n));
        return;
    }
    if (span_is(op, "IDX_TEXT")) {
        expect_ok(corvid_create_text_index(docs(), a[0].p, a[0].n));
        return;
    }
    if (span_is(op, "IDX_TEXT_DISK")) {
        expect_ok(corvid_create_text_index_ondisk(docs(), a[0].p, a[0].n));
        return;
    }
    if (span_is(op, "IDX_GEO")) {
        expect_ok(corvid_create_geo_index(docs(), a[0].p, a[0].n));
        return;
    }
    if (span_is(op, "IDX_VEC")) {
        expect_ok(corvid_create_vector_index(docs(), a[0].p, a[0].n,
                                             parse_metric(a[1])));
        return;
    }
    if (span_is(op, "IDX_VEC_Q")) {
        expect_ok(corvid_create_vector_index_quantized(
            docs(), a[0].p, a[0].n, parse_metric(a[1]), parse_quant(a[2])));
        return;
    }
    if (span_is(op, "IDX_VEC_DISK")) {
        expect_ok(corvid_create_vector_index_ondisk(docs(), a[0].p, a[0].n,
                                                    parse_metric(a[1])));
        return;
    }
    if (span_is(op, "IDX_VEC_DISK_Q")) {
        expect_ok(corvid_create_vector_index_ondisk_quantized(
            docs(), a[0].p, a[0].n, parse_metric(a[1]), parse_quant(a[2])));
        return;
    }
    if (span_is(op, "IDX_PQ") || span_is(op, "IDX_PQ_DISK") ||
        span_is(op, "IDX_PQ_ERR")) {
        corvid_status st;
        if (span_is(op, "IDX_PQ_DISK"))
            st = corvid_create_vector_index_ondisk_pq(
                docs(), a[0].p, a[0].n, parse_metric(a[1]),
                (size_t)parse_i64(a[2]), (size_t)parse_i64(a[3]));
        else
            st = corvid_create_vector_index_pq(
                docs(), a[0].p, a[0].n, parse_metric(a[1]),
                (size_t)parse_i64(a[2]), (size_t)parse_i64(a[3]));
        if (span_is(op, "IDX_PQ_ERR")) {
            CHECK(st == CORVID_ERR, "pq create unexpectedly succeeded");
            expect_err(st, err_token(expected));
        } else {
            expect_ok(st);
        }
        return;
    }

    /* ---- admin & persistence ---- */
    if (span_is(op, "FILEDB")) {
        open_file(S.db_path);
        return;
    }
    if (span_is(op, "FILEDB2")) {
        open_file(S.db2_path);
        return;
    }
    if (span_is(op, "DUMP")) {
        expect_ok(
            corvid_dump_to_path(S.db, S.dump_path, strlen(S.dump_path)));
        return;
    }
    if (span_is(op, "LOAD")) {
        expect_ok(
            corvid_load_from_path(S.db, S.dump_path, strlen(S.dump_path)));
        return;
    }
    if (span_is(op, "LOAD_RENAMES")) {
        const char *olds[2] = {a[0].p, NULL};
        const char *news[2] = {a[1].p, NULL};
        size_t olens[2] = {a[0].n, 0};
        size_t nlens[2] = {a[1].n, 0};
        corvid_status st = corvid_load_from_path_with_renames(
            S.db, S.dump_path, strlen(S.dump_path), olds, news, olens, nlens,
            1);
        if (expected.n > 4 && memcmp(expected.p, "err:", 4) == 0)
            expect_err(st, err_token(expected));
        else
            expect_ok(st);
        return;
    }
    if (span_is(op, "COLLECTIONS")) {
        corvid_strs *s = corvid_collections(S.db);
        CHECK(s != NULL, "corvid_collections failed");
        RowWalk w;
        w.n = 0;
        for (;;) {
            const char *item = NULL;
            size_t ilen = 0;
            if (corvid_strs_next(s, &item, &ilen) != 1) break;
            CHECK(w.n < MAX_ROWS, "collections overflow");
            CHECK(ilen < sizeof w.key_buf[0], "collection name too long");
            memcpy(w.key_buf[w.n], item, ilen);
            w.key_buf[w.n][ilen] = 0;
            w.keys[w.n] = (Span){w.key_buf[w.n], ilen};
            w.n++;
        }
        corvid_strs_free(s);
        check_keys(&w, expected);
        return;
    }
    if (span_is(op, "BACKUP")) {
        expect_ok(corvid_backup(S.db, S.backup_path, strlen(S.backup_path)));
        return;
    }
    if (span_is(op, "BACKUP_DUP")) {
        corvid_status st =
            corvid_backup(S.db, S.backup_path, strlen(S.backup_path));
        expect_err(st, CORVID_E_BACKUP_TARGET_EXISTS);
        return;
    }
    if (span_is(op, "COMPACT_BUSY")) {
        corvid_status st = corvid_compact(S.db, NULL);
        expect_err(st, CORVID_E_BUSY);
        return;
    }
    if (span_is(op, "COMPACT")) {
        close_coll(); /* quiesce: the derived-handle gate (§4.13) */
        int moved = -1;
        expect_ok(corvid_compact(S.db, &moved));
        CHECK(moved == 0 || moved == 1, "moved_out must be boolean");
        (void)docs(); /* re-acquire for subsequent lines */
        return;
    }
    if (span_is(op, "REOPEN")) {
        char path[512];
        snprintf(path, sizeof path, "%s", S.db_path);
        close_db();
        S.db = corvid_open(path, strlen(path));
        CHECK(S.db != NULL, "reopen of %s failed", path);
        (void)docs();
        return;
    }

    fail("unknown OP '%.*s'", (int)op.n, op.p);
}

/* ------------------------------------------------------------------ */
/* Fixture-file driver                                                 */
/* ------------------------------------------------------------------ */

/* values.txt runs against no db; every other file starts in-memory
 * (admin/persist switch to file dbs via their OPs). */
static int starts_with_db(const char *path) {
    const char *base = strrchr(path, '/');
    base = base ? base + 1 : path;
    return strcmp(base, "values.txt") != 0;
}

static void run_scenario(const char *path) {
    FILE *f = fopen(path, "rb");
    CHECK(f != NULL, "cannot open fixture %s", path);
    fseek(f, 0, SEEK_END);
    long size = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (size < 0) fail("ftell failed on %s", path);
    char *buf = malloc((size_t)size + 1);
    CHECK(buf != NULL, "out of memory reading %s", path);
    if (fread(buf, 1, (size_t)size, f) != (size_t)size)
        fail("short read on %s", path);
    buf[size] = 0;
    fclose(f);

    g_file = path;
    g_line = 0;
    S.db = NULL;
    S.coll = NULL;
    S.last_auto_id = 0;
    /* Scratch paths are per-scenario (keyed on the fixture basename) so
     * file-db scenarios sharing one workdir never touch each other's
     * files. */
    {
        /* Either separator: on Windows the driver passes backslash
         * paths, and missing this made the whole absolute path the
         * "stem" — workdir + "/D:\a\..." is not a legal path there
         * (unix tolerated the prefix by accident). */
        const char *base = strrchr(path, '/');
        const char *back = strrchr(path, '\\');
        if (back && (!base || back > base)) base = back;
        base = base ? base + 1 : path;
        char stem[256];
        size_t sn = strlen(base);
        const char *dot = strrchr(base, '.');
        if (dot && dot > base) sn = (size_t)(dot - base);
        if (sn >= sizeof stem) sn = sizeof stem - 1;
        memcpy(stem, base, sn);
        stem[sn] = 0;
        snprintf(S.db_path, sizeof S.db_path, "%s/%s.redb", S.workdir, stem);
        snprintf(S.db2_path, sizeof S.db2_path, "%s/%s-2.redb", S.workdir, stem);
        snprintf(S.dump_path, sizeof S.dump_path, "%s/%s.dump", S.workdir, stem);
        snprintf(S.backup_path, sizeof S.backup_path, "%s/%s.backup.redb",
                 S.workdir, stem);
    }
    if (starts_with_db(path)) open_memory();

    /* `lines` is counted in an INDEPENDENT pre-scan (the same rule the
     * Rust driver applies), so a dispatch loop that skips a counted
     * line — a stray `continue`, a swallowed branch — diverges from
     * `executed` and the check below reports it, instead of the two
     * fields silently reading one counter. The scan is non-destructive
     * (no NUL writes): the dispatch pass re-splits the same buffer. */
    long lines = 0;
    for (const char *q = buf; q && *q;) {
        const char *nl = strchr(q, '\n');
        const char *end = nl ? nl : q + strlen(q);
        /* skip spaces and a trailing \r, then apply the same executable
         * rule: blank or '#' lines are not executable. */
        const char *first = q;
        while (first < end && (*first == ' ' || *first == '\r')) first++;
        if (first < end && *first != '#') lines++;
        q = nl ? nl + 1 : NULL;
    }

    long executed = 0;
    char *p = buf;
    while (p && *p) {
        char *nl = strchr(p, '\n');
        char *line = p;
        if (nl) *nl = 0;
        p = nl ? nl + 1 : NULL;

        size_t len = strlen(line);
        while (len && line[len - 1] == '\r') line[--len] = 0;
        if (len == 0 || line[0] == '#') continue;
        g_line++;

        /* OP \t ARGS \t EXPECTED */
        char *tab1 = strchr(line, '\t');
        char *tab2 = tab1 ? strchr(tab1 + 1, '\t') : NULL;
        Span op, args, expected;
        if (tab1 == NULL) {
            op = (Span){line, len};
            args = (Span){line, 0};
            expected = (Span){line, 0};
        } else if (tab2 == NULL) {
            *tab1 = 0;
            op = (Span){line, strlen(line)};
            args = (Span){tab1 + 1, strlen(tab1 + 1)};
            expected = (Span){line, 0};
        } else {
            *tab1 = 0;
            *tab2 = 0;
            op = (Span){line, strlen(line)};
            args = (Span){tab1 + 1, (size_t)(tab2 - tab1 - 1)};
            expected = (Span){tab2 + 1, strlen(tab2 + 1)};
        }
        {
            static char opbuf[32];
            size_t on = op.n < sizeof opbuf - 1 ? op.n : sizeof opbuf - 1;
            memcpy(opbuf, op.p, on);
            opbuf[on] = 0;
            g_op = opbuf;
        }
        run_line(op, args, expected);
        executed++;
    }
    free(buf);
    close_db();
    if (executed != lines)
        fail("dispatched %ld of %ld counted executable lines", executed, lines);
    printf("SMOKE %s lines=%ld executed=%ld\n", path, lines, executed);
    fflush(stdout);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <workdir> <fixture.txt> [fixture.txt ...]\n",
                argv[0]);
        return 2;
    }
    snprintf(S.workdir, sizeof S.workdir, "%s", argv[1]);

    if (corvid_ffi_version() != 1) {
        fprintf(stderr, "FAIL wrong FFI_VERSION %u\n", corvid_ffi_version());
        return 1;
    }
    for (int i = 2; i < argc; i++) run_scenario(argv[i]);
    return 0;
}
