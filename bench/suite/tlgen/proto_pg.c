/* PostgreSQL, protocol 3.0, a simple query ('Q').
 *
 * Authentication is trust or cleartext. md5/scram are deliberately NOT implemented:
 * the benchmark stand is configured with trust, and a silent failure while parsing
 * SCRAM would look like "the server does not answer". */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "tlgen.h"

typedef struct { int sent_startup, sent_pass; } pg_st;

static void put32(char *p, uint32_t v) {
    p[0] = (char)(v >> 24); p[1] = (char)(v >> 16); p[2] = (char)(v >> 8); p[3] = (char)v;
}
static uint32_t get32(const char *p) {
    return ((uint32_t)(unsigned char)p[0] << 24) | ((uint32_t)(unsigned char)p[1] << 16) |
           ((uint32_t)(unsigned char)p[2] << 8) | (uint32_t)(unsigned char)p[3];
}

static int p_init(tl_conn *c, const tl_opts *o) {
    (void)o;
    c->ps = calloc(1, sizeof(pg_st));
    return c->ps ? 0 : -1;
}
static void p_free(tl_conn *c) { free(c->ps); c->ps = NULL; }

static int p_hs(tl_conn *c, const tl_opts *o, const char *in, size_t inlen,
                size_t *consumed, char *out, size_t outcap, size_t *outlen) {
    pg_st *s = c->ps;
    *consumed = 0; *outlen = 0;

    if (!s->sent_startup) {
        s->sent_startup = 1;
        char body[512];
        int n = 0;
        n += snprintf(body + n, sizeof(body) - (size_t)n, "user") + 1;
        n += snprintf(body + n, sizeof(body) - (size_t)n, "%s", o->user) + 1;
        n += snprintf(body + n, sizeof(body) - (size_t)n, "database") + 1;
        n += snprintf(body + n, sizeof(body) - (size_t)n, "%s", o->db) + 1;
        body[n++] = 0;
        if ((size_t)(n + 8) > outcap) return -1;
        put32(out, (uint32_t)(n + 8));
        put32(out + 4, 196608);                   /* 3.0 */
        memcpy(out + 8, body, (size_t)n);
        *outlen = (size_t)n + 8;
        return 1;
    }

    /* we parse the server's messages until ReadyForQuery arrives */
    size_t off = 0;
    while (off + 5 <= inlen) {
        char t = in[off];
        uint32_t len = get32(in + off + 1);
        if (len < 4 || off + 1 + len > inlen) break;
        const char *body = in + off + 5;
        uint32_t blen = len - 4;
        off += 1 + len;
        if (t == 'R') {
            uint32_t code = blen >= 4 ? get32(body) : 0;
            if (code == 0) continue;              /* AuthenticationOk */
            if (code == 3) {                      /* a cleartext password */
                if (s->sent_pass) return -1;
                s->sent_pass = 1;
                size_t pl = strlen(o->pass) + 1;
                if (pl + 5 > outcap) return -1;
                out[0] = 'p'; put32(out + 1, (uint32_t)(pl + 4));
                memcpy(out + 5, o->pass, pl);
                *outlen = pl + 5;
                *consumed = off;
                return 1;
            }
            tl_diag(c, "pg: the server requires authentication of type %u — the stand must be configured for trust", code);
            return -1;
        }
        if (t == 'E') {
            /* ErrorResponse: a sequence of "code + string" fields, terminated by
             * a zero code. We are interested in severity(S), SQLSTATE(C) and the text(M):
             * that is where "sorry, too many clients already" or
             * "the database system is in recovery mode" is written. Without that text
             * a server's refusal was indistinguishable from a network failure. */
            const char *sev = "", *sqlst = "", *msg = "";
            for (uint32_t k = 0; k + 1 < blen; ) {
                char f = body[k];
                if (!f) break;
                const char *v = body + k + 1;
                uint32_t vl = 0;
                while (k + 1 + vl < blen && v[vl]) vl++;
                if (f == 'S') sev = v;
                else if (f == 'C') sqlst = v;
                else if (f == 'M') msg = v;
                k += 1 + vl + 1;
            }
            tl_diag(c, "pg: the server refused the connection: %s %s %s", sev, sqlst, msg);
            *consumed = off; return -1;
        }
        if (t == 'Z') { *consumed = off; return 0; }
    }
    *consumed = off;
    return 1;
}

static int p_build(tl_conn *c, const tl_opts *o, uint64_t seq, char *out, size_t cap) {
    (void)c; (void)seq;
    size_t q = strlen(o->query) + 1;
    if (q + 5 > cap) return -1;
    out[0] = 'Q'; put32(out + 1, (uint32_t)(q + 4));
    memcpy(out + 5, o->query, q);
    return (int)(q + 5);
}

static int p_parse(tl_conn *c, const tl_opts *o, const char *in, size_t len, int *is_err) {
    (void)c; (void)o;
    size_t off = 0;
    while (off + 5 <= len) {
        char t = in[off];
        uint32_t l = get32(in + off + 1);
        if (l < 4 || off + 1 + l > len) return 0;
        off += 1 + l;
        if (t == 'E') *is_err = 1;
        if (t == 'Z') return (int)off;
    }
    return 0;
}

static const tl_proto PG = { "pg", p_init, p_hs, p_build, p_parse, p_free };
const tl_proto *tl_proto_pg(void) { return &PG; }
