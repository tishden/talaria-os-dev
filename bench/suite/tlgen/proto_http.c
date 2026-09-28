/* HTTP/1.1 keep-alive. Used for nginx/mongoose (--op get) and for
 * the ClickHouse HTTP interface (--op post: the query goes in the body). */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "tlgen.h"

typedef struct { uint64_t rnd; } http_st;

static int h_init(tl_conn *c, const tl_opts *o) {
    http_st *s = calloc(1, sizeof(*s));
    if (!s) return -1;
    s->rnd = 0x2545F4914F6CDD1Dull ^ ((uint64_t)c->idx * 7919u + o->seed);
    c->ps = s;
    return 0;
}
static void h_free(tl_conn *c) { free(c->ps); c->ps = NULL; }

static int h_hs(tl_conn *c, const tl_opts *o, const char *in, size_t inlen,
                size_t *consumed, char *out, size_t outcap, size_t *outlen) {
    (void)c; (void)o; (void)in; (void)inlen; (void)out; (void)outcap;
    *consumed = 0; *outlen = 0;
    return 0;                                    /* there is no handshake */
}

static int h_build(tl_conn *c, const tl_opts *o, uint64_t seq, char *out, size_t cap) {
    (void)c; (void)seq;
    if (!strcmp(o->op, "post")) {
        size_t qlen = strlen(o->query);
        return snprintf(out, cap,
            "POST /?default_format=TabSeparated HTTP/1.1\r\nHost: %s\r\n"
            "User-Agent: tlgen\r\nContent-Length: %zu\r\nConnection: keep-alive\r\n\r\n%s",
            o->host, qlen, o->query);
    }
    return snprintf(out, cap,
        "GET %s HTTP/1.1\r\nHost: %s\r\nUser-Agent: tlgen\r\n"
        "Accept: */*\r\nConnection: keep-alive\r\n\r\n", o->path, o->host);
}

/* One answer: the headers + the body by Content-Length or chunked. */
static int h_parse(tl_conn *c, const tl_opts *o, const char *in, size_t len, int *is_err) {
    (void)c; (void)o;
    if (len < 16) return 0;
    const char *hdr_end = memmem(in, len, "\r\n\r\n", 4);
    if (!hdr_end) return 0;
    size_t hlen = (size_t)(hdr_end - in) + 4;

    if (len >= 12 && !memcmp(in, "HTTP/1.", 7)) {
        int code = (in[9] - '0') * 100 + (in[10] - '0') * 10 + (in[11] - '0');
        if (code >= 400) *is_err = 1;
    } else return -1;

    /* Content-Length (any case — we look for both usual spellings) */
    const char *cl = memmem(in, hlen, "\r\nContent-Length:", 17);
    if (!cl) cl = memmem(in, hlen, "\r\ncontent-length:", 17);
    if (cl) {
        long n = strtol(cl + 17, NULL, 10);
        if (n < 0) return -1;
        return len >= hlen + (size_t)n ? (int)(hlen + (size_t)n) : 0;
    }
    const char *te = memmem(in, hlen, "\r\nTransfer-Encoding: chunked", 27);
    if (!te) te = memmem(in, hlen, "\r\ntransfer-encoding: chunked", 27);
    if (te) {
        size_t off = hlen;
        for (;;) {
            const char *nl = memchr(in + off, '\n', len - off);
            if (!nl) return 0;
            long sz = strtol(in + off, NULL, 16);
            size_t after = (size_t)(nl - in) + 1;
            if (sz == 0) {                       /* the terminating chunk + trailers */
                const char *end = memmem(in + after - 2, len - after + 2, "\r\n\r\n", 4);
                if (!end) return len >= after + 2 ? (int)(after + 2) : 0;
                return (int)(end - in) + 4;
            }
            if (len < after + (size_t)sz + 2) return 0;
            off = after + (size_t)sz + 2;
        }
    }
    /* neither one nor the other: an answer without a body (204/304/HEAD) */
    return (int)hlen;
}

static const tl_proto HTTP = { "http", h_init, h_hs, h_build, h_parse, h_free };
const tl_proto *tl_proto_http(void) { return &HTTP; }
