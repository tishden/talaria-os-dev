/* RESP (Redis/Valkey/KeyDB). Operations: ping, get, set, incr, mixed. */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "tlgen.h"

typedef struct { uint64_t rnd; int authed; } resp_st;

static int r_init(tl_conn *c, const tl_opts *o) {
    resp_st *s = calloc(1, sizeof(*s));
    if (!s) return -1;
    s->rnd = 0x9E3779B97F4A7C15ull ^ ((uint64_t)c->idx * 1000003u + o->seed);
    c->ps = s;
    return 0;
}
static void r_free(tl_conn *c) { free(c->ps); c->ps = NULL; }

static int r_hs(tl_conn *c, const tl_opts *o, const char *in, size_t inlen,
                size_t *consumed, char *out, size_t outcap, size_t *outlen) {
    resp_st *s = c->ps;
    *consumed = 0; *outlen = 0;
    if (!o->pass || !o->pass[0]) return 0;
    if (!s->authed) {
        s->authed = 1;
        *outlen = (size_t)snprintf(out, outcap, "*2\r\n$4\r\nAUTH\r\n$%zu\r\n%s\r\n",
                                   strlen(o->pass), o->pass);
        return 1;
    }
    /* we wait for +OK */
    const char *e = memchr(in, '\n', inlen);
    if (!e) return 1;
    *consumed = (size_t)(e - in) + 1;
    return in[0] == '-' ? -1 : 0;
}

static int r_build(tl_conn *c, const tl_opts *o, uint64_t seq, char *out, size_t cap) {
    resp_st *s = c->ps;
    (void)seq;
    int op_get;
    if      (!strcmp(o->op, "ping")) {
        return snprintf(out, cap, "*1\r\n$4\r\nPING\r\n");
    }
    else if (!strcmp(o->op, "get"))   op_get = 1;
    else if (!strcmp(o->op, "set"))   op_get = 0;
    else if (!strcmp(o->op, "incr"))  op_get = 2;
    else /* mixed */                  op_get = ((int)(tl_rnd(&s->rnd) % 100) < o->read_pct);

    unsigned k = (unsigned)(tl_rnd(&s->rnd) % (unsigned)(o->keyspace > 0 ? o->keyspace : 1));
    char key[64];
    int klen = snprintf(key, sizeof(key), "tlgen:%08u", k);

    if (op_get == 2)
        return snprintf(out, cap, "*2\r\n$4\r\nINCR\r\n$%d\r\n%s\r\n", klen, key);
    if (op_get)
        return snprintf(out, cap, "*2\r\n$3\r\nGET\r\n$%d\r\n%s\r\n", klen, key);

    int vlen = o->valsize > 0 ? o->valsize : 1;
    if ((size_t)(64 + klen + vlen) > cap) return -1;
    int n = snprintf(out, cap, "*3\r\n$3\r\nSET\r\n$%d\r\n%s\r\n$%d\r\n", klen, key, vlen);
    memset(out + n, 'x', (size_t)vlen);
    n += vlen;
    out[n++] = '\r'; out[n++] = '\n';
    return n;
}

/* Parsing one RESP2 value. >0 — the length, 0 — incomplete, <0 — garbage. */
static int r_one(const char *p, size_t len, int *is_err) {
    if (!len) return 0;
    const char *nl;
    switch (p[0]) {
    case '+': case ':':
        nl = memchr(p, '\n', len);
        return nl ? (int)(nl - p) + 1 : 0;
    case '-':
        nl = memchr(p, '\n', len);
        if (!nl) return 0;
        *is_err = 1;
        return (int)(nl - p) + 1;
    case '$': {
        nl = memchr(p, '\n', len);
        if (!nl) return 0;
        long n = strtol(p + 1, NULL, 10);
        size_t hdr = (size_t)(nl - p) + 1;
        if (n < 0) return (int)hdr;                     /* nil */
        if (len < hdr + (size_t)n + 2) return 0;
        return (int)(hdr + (size_t)n + 2);
    }
    case '*': {
        nl = memchr(p, '\n', len);
        if (!nl) return 0;
        long n = strtol(p + 1, NULL, 10);
        size_t off = (size_t)(nl - p) + 1;
        if (n < 0) return (int)off;
        for (long i = 0; i < n; i++) {
            int k = r_one(p + off, len - off, is_err);
            if (k <= 0) return k;
            off += (size_t)k;
        }
        return (int)off;
    }
    default:
        return -1;
    }
}

static int r_parse(tl_conn *c, const tl_opts *o, const char *in, size_t len, int *is_err) {
    (void)c; (void)o;
    return r_one(in, len, is_err);
}

static const tl_proto RESP = { "resp", r_init, r_hs, r_build, r_parse, r_free };
const tl_proto *tl_proto_resp(void) { return &RESP; }
