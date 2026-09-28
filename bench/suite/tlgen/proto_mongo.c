/* MongoDB: OP_MSG (opcode 2013, present since 3.6). Operations: ping, find, insert.
 * There is no handshake — mongod accepts commands right away. */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "tlgen.h"

typedef struct { uint64_t rnd; uint32_t reqid; } mg_st;

/* THE RUN NUMBER in the insert key. `_id` must be unique IN THE COLLECTION, and
 * the collection is not cleaned between runs: without this the second run would insert
 * the same keys and get writeErrors (duplicate key) — i.e. it would measure a failure,
 * and at `ok: 1.0` at that, invisible to the answer check. The number comes from the
 * job (tl_job_nonce), NOT from the process: in --serve the process is one for the whole
 * stand, and "once per process" meant "the same keys for every point of the suite". */
#define g_nonce tl_job_nonce

static void le32(char *p, uint32_t v) {
    p[0] = (char)v; p[1] = (char)(v >> 8); p[2] = (char)(v >> 16); p[3] = (char)(v >> 24);
}
static uint32_t rd32(const char *p) {
    return (uint32_t)(unsigned char)p[0] | ((uint32_t)(unsigned char)p[1] << 8) |
           ((uint32_t)(unsigned char)p[2] << 16) | ((uint32_t)(unsigned char)p[3] << 24);
}

/* --- a minimal BSON builder: as much as the commands below need --- */
typedef struct { char *p; size_t n, cap; } bson;

static void b_start(bson *b, char *buf, size_t cap) { b->p = buf; b->cap = cap; b->n = 4; }
static void b_raw(bson *b, const void *d, size_t n) {
    if (b->n + n <= b->cap) { memcpy(b->p + b->n, d, n); b->n += n; }
}
static void b_name(bson *b, char type, const char *nm) {
    b_raw(b, &type, 1); b_raw(b, nm, strlen(nm) + 1);
}
static void b_str(bson *b, const char *nm, const char *v) {
    b_name(b, 0x02, nm);
    uint32_t l = (uint32_t)strlen(v) + 1;
    char t[4]; le32(t, l); b_raw(b, t, 4); b_raw(b, v, l);
}
static void b_i32(bson *b, const char *nm, int32_t v) {
    b_name(b, 0x10, nm);
    char t[4]; le32(t, (uint32_t)v); b_raw(b, t, 4);
}
static void b_i64(bson *b, const char *nm, int64_t v) {
    b_name(b, 0x12, nm);
    char t[8]; le32(t, (uint32_t)v); le32(t + 4, (uint32_t)((uint64_t)v >> 32)); b_raw(b, t, 8);
}
/* A BOOLEAN FIELD (0x08). A separate type rather than int32: mongod's type checking
 * is strict, and `ordered` as a number is rejected ENTIRELY — "BSON field
 * 'insert.ordered' is the wrong type 'int', expected type 'bool'" (code 14,
 * TypeMismatch). Because of that the insert measurement measured NOTHING: the server answered
 * ok: 0.0 to every request, i.e. the round trip of a failure was measured. */
static void b_bool(bson *b, const char *nm, int v) {
    b_name(b, 0x08, nm);
    char t = (char)(v ? 1 : 0); b_raw(b, &t, 1);
}
static void b_empty_doc(bson *b, const char *nm) {
    b_name(b, 0x03, nm);
    char t[5] = {5, 0, 0, 0, 0}; b_raw(b, t, 5);
}
static size_t b_end(bson *b) {
    char z = 0; b_raw(b, &z, 1);
    le32(b->p, (uint32_t)b->n);
    return b->n;
}

static int g_init(tl_conn *c, const tl_opts *o) {
    mg_st *s = calloc(1, sizeof(*s));
    if (!s) return -1;
    s->rnd = 0xD1B54A32D192ED03ull ^ ((uint64_t)c->idx * 104729u + o->seed);
    s->reqid = (uint32_t)c->idx * 1000000u + 1;
    c->ps = s;
    return 0;
}
static void g_free(tl_conn *c) { free(c->ps); c->ps = NULL; }

static int g_hs(tl_conn *c, const tl_opts *o, const char *in, size_t inlen,
                size_t *consumed, char *out, size_t outcap, size_t *outlen) {
    (void)c; (void)o; (void)in; (void)inlen; (void)out; (void)outcap;
    *consumed = 0; *outlen = 0;
    return 0;
}

static int g_build(tl_conn *c, const tl_opts *o, uint64_t seq, char *out, size_t cap) {
    mg_st *s = c->ps;
    if (cap < 512) return -1;
    char doc[4096];
    bson b; b_start(&b, doc, sizeof(doc));

    if (!strcmp(o->op, "insert")) {
        b_str(&b, "insert", o->coll);
        /* documents: [ { _id: <seq>, v: "..." } ] — an array in BSON is a document
         * with the keys "0", "1", ... */
        b_name(&b, 0x04, "documents");
        char arr[2048]; bson a; b_start(&a, arr, sizeof(arr));
        b_name(&a, 0x03, "0");
        char d0[1024]; bson d; b_start(&d, d0, sizeof(d0));
        /* the key = {run number, connection, request number}: unique both within
         * a run and between runs (see g_nonce). */
        b_i64(&d, "_id", (int64_t)((g_nonce << 20) ^ (seq * 1000 + (uint64_t)c->idx)));
        int vl = o->valsize > 0 && o->valsize < 900 ? o->valsize : 64;
        char v[1024]; memset(v, 'x', (size_t)vl); v[vl] = 0;
        b_str(&d, "v", v);
        size_t dn = b_end(&d);
        b_raw(&a, d0, dn);
        size_t an = b_end(&a);
        b_raw(&b, arr, an);
        b_bool(&b, "ordered", 0);
    } else if (!strcmp(o->op, "find")) {
        b_str(&b, "find", o->coll);
        b_empty_doc(&b, "filter");
        b_i32(&b, "limit", 1);
    } else {
        b_i32(&b, "ping", 1);
    }
    b_str(&b, "$db", !strcmp(o->op, "ping") ? "admin" : o->db);
    size_t dn = b_end(&b);

    uint32_t total = 16 + 4 + 1 + (uint32_t)dn;
    if (cap < total) return -1;
    le32(out, total);
    le32(out + 4, s->reqid++);
    le32(out + 8, 0);
    le32(out + 12, 2013);                 /* OP_MSG */
    le32(out + 16, 0);                    /* flagBits */
    out[20] = 0;                          /* section kind 0 */
    memcpy(out + 21, doc, dn);
    return (int)total;
}

static int g_parse(tl_conn *c, const tl_opts *o, const char *in, size_t len, int *is_err) {
    (void)c; (void)o;
    if (len < 16) return 0;
    uint32_t total = rd32(in);
    if (total < 16 || total > (1u << 26)) return -1;
    if (len < total) return 0;
    /* ok: 0.0 in the answer — a double element named "ok" with a zero */
    static const unsigned char okzero[12] = {0x01, 'o', 'k', 0, 0, 0, 0, 0, 0, 0, 0, 0};
    if (memmem(in, total, okzero, sizeof(okzero))) *is_err = 1;
    /* A WRITE FAILURE ARRIVES WITH ok: 1.0. The insert command answers "executed
     * successfully", while the documents themselves may be rejected — the list is in the
     * writeErrors field (a duplicate key, the schema, a limit). Without this check the insert
     * measurement would count failures as successful requests and would show a round trip in which
     * there was no write. */
    static const unsigned char werr[13] = {0x04, 'w', 'r', 'i', 't', 'e', 'E', 'r', 'r', 'o', 'r', 's', 0};
    if (memmem(in, total, werr, sizeof(werr))) *is_err = 1;
    return (int)total;
}

static const tl_proto MG = { "mongo", g_init, g_hs, g_build, g_parse, g_free };
const tl_proto *tl_proto_mongo(void) { return &MG; }
