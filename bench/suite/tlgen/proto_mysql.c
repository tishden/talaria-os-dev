/* MySQL/MariaDB: the v10 handshake + mysql_native_password, COM_QUERY requests.
 *
 * CLIENT_DEPRECATE_EOF is deliberately NOT declared: with the old framing (EOF packets)
 * the answer's boundary is determined unambiguously and identically on MySQL and MariaDB. */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "tlgen.h"

/* ----------------------------------------------------------------- SHA-1 */
typedef struct { uint32_t h[5]; uint64_t n; unsigned char b[64]; size_t bl; } sha1_t;

static uint32_t rol(uint32_t v, int s) { return (v << s) | (v >> (32 - s)); }

static void sha1_block(sha1_t *c, const unsigned char *p) {
    uint32_t w[80], a, b, d, e, f, k, t;
    for (int i = 0; i < 16; i++)
        w[i] = ((uint32_t)p[i*4] << 24) | ((uint32_t)p[i*4+1] << 16) |
               ((uint32_t)p[i*4+2] << 8) | (uint32_t)p[i*4+3];
    for (int i = 16; i < 80; i++) w[i] = rol(w[i-3] ^ w[i-8] ^ w[i-14] ^ w[i-16], 1);
    a = c->h[0]; b = c->h[1]; uint32_t cc = c->h[2]; d = c->h[3]; e = c->h[4];
    for (int i = 0; i < 80; i++) {
        if      (i < 20) { f = (b & cc) | (~b & d);            k = 0x5A827999; }
        else if (i < 40) { f = b ^ cc ^ d;                     k = 0x6ED9EBA1; }
        else if (i < 60) { f = (b & cc) | (b & d) | (cc & d);  k = 0x8F1BBCDC; }
        else             { f = b ^ cc ^ d;                     k = 0xCA62C1D6; }
        t = rol(a, 5) + f + e + k + w[i];
        e = d; d = cc; cc = rol(b, 30); b = a; a = t;
    }
    c->h[0] += a; c->h[1] += b; c->h[2] += cc; c->h[3] += d; c->h[4] += e;
}
static void sha1_init(sha1_t *c) {
    c->h[0]=0x67452301; c->h[1]=0xEFCDAB89; c->h[2]=0x98BADCFE;
    c->h[3]=0x10325476; c->h[4]=0xC3D2E1F0; c->n=0; c->bl=0;
}
static void sha1_up(sha1_t *c, const void *p, size_t n) {
    const unsigned char *q = p; c->n += n;
    while (n) {
        size_t k = 64 - c->bl; if (k > n) k = n;
        memcpy(c->b + c->bl, q, k); c->bl += k; q += k; n -= k;
        if (c->bl == 64) { sha1_block(c, c->b); c->bl = 0; }
    }
}
static void sha1_fin(sha1_t *c, unsigned char out[20]) {
    uint64_t bits = c->n * 8;
    unsigned char pad = 0x80;
    sha1_up(c, &pad, 1);
    unsigned char z = 0;
    while (c->bl != 56) sha1_up(c, &z, 1);
    unsigned char l[8];
    for (int i = 0; i < 8; i++) l[i] = (unsigned char)(bits >> (56 - 8*i));
    sha1_up(c, l, 8);
    for (int i = 0; i < 5; i++) {
        out[i*4]   = (unsigned char)(c->h[i] >> 24); out[i*4+1] = (unsigned char)(c->h[i] >> 16);
        out[i*4+2] = (unsigned char)(c->h[i] >> 8);  out[i*4+3] = (unsigned char)c->h[i];
    }
}
static void sha1(const void *p, size_t n, unsigned char out[20]) {
    sha1_t c; sha1_init(&c); sha1_up(&c, p, n); sha1_fin(&c, out);
}

/* ------------------------------------------------------------- state */
typedef struct {
    int  stage;              /* 0 — awaiting the greeting, 1 — awaiting OK, 2 — ready */
    int  rs;                 /* answer parsing: 0 the first packet, 1 the columns, 2 the rows */
    int  cols_left, eofs;
    unsigned char salt[21];
    int  saltlen;
} my_st;

static int m_init(tl_conn *c, const tl_opts *o) {
    (void)o;
    c->ps = calloc(1, sizeof(my_st));
    return c->ps ? 0 : -1;
}
static void m_free(tl_conn *c) { free(c->ps); c->ps = NULL; }

static size_t pktlen(const char *p) {
    return (size_t)(unsigned char)p[0] | ((size_t)(unsigned char)p[1] << 8) |
           ((size_t)(unsigned char)p[2] << 16);
}

static int m_hs(tl_conn *c, const tl_opts *o, const char *in, size_t inlen,
                size_t *consumed, char *out, size_t outcap, size_t *outlen) {
    my_st *s = c->ps;
    *consumed = 0; *outlen = 0;

    if (inlen < 4) return 1;
    size_t pl = pktlen(in);
    if (inlen < 4 + pl) return 1;
    const unsigned char *p = (const unsigned char *)in + 4;

    if (s->stage == 0) {
        /* ERR instead of a greeting — the server refuses BEFORE login: "Too many
         * connections", "Host is blocked". The text is written by the server itself, and without it
         * the refusal looked like a network disconnect. */
        if (p[0] == 0xFF) {
            tl_diag(c, "mysql: the server refused the connection before login: %.*s",
                    (int)(pl > 9 ? pl - 9 : 0), (const char *)p + 9);
            return -1;
        }
        if (p[0] != 10) { tl_diag(c, "mysql: greeting protocol v%u is not supported (v10 expected)", p[0]); return -1; }
        size_t i = 1;
        while (i < pl && p[i]) i++;                      /* the server version */
        i++;
        i += 4;                                          /* connection id */
        memcpy(s->salt, p + i, 8); s->saltlen = 8; i += 8;
        i += 1;                                          /* filler */
        i += 2;                                          /* capability lower */
        if (i < pl) {
            i += 1 + 2 + 2;                              /* charset, status, capability upper */
            int alen = (int)p[i]; i += 1;
            i += 10;                                     /* reserved */
            int rest = alen > 8 ? alen - 8 : 13;
            if (rest > 13) rest = 13;
            if (i + (size_t)rest <= pl) {
                memcpy(s->salt + 8, p + i, (size_t)(rest > 12 ? 12 : rest));
                s->saltlen = 8 + (rest > 12 ? 12 : rest);
            }
        }
        *consumed = 4 + pl;

        /* the client's answer */
        unsigned char scram[20];
        int slen = 0;
        if (o->pass && o->pass[0]) {
            unsigned char h1[20], h2[20], h3[20];
            sha1(o->pass, strlen(o->pass), h1);
            sha1(h1, 20, h2);
            sha1_t x; sha1_init(&x);
            sha1_up(&x, s->salt, (size_t)s->saltlen);
            sha1_up(&x, h2, 20);
            sha1_fin(&x, h3);
            for (int k = 0; k < 20; k++) scram[k] = h1[k] ^ h3[k];
            slen = 20;
        }
        /* CLIENT_LONG_PASSWORD|LONG_FLAG|CONNECT_WITH_DB|PROTOCOL_41|
           TRANSACTIONS|SECURE_CONNECTION|PLUGIN_AUTH */
        uint32_t caps = 0x00000001u | 0x00000004u | 0x00000008u | 0x00000200u |
                        0x00002000u | 0x00008000u | 0x00080000u;
        char body[512]; size_t n = 0;
        body[n++] = (char)caps; body[n++] = (char)(caps >> 8);
        body[n++] = (char)(caps >> 16); body[n++] = (char)(caps >> 24);
        body[n++] = 0; body[n++] = 0; body[n++] = 0x01; body[n++] = 0;   /* 16 MB */
        body[n++] = 45;                                                  /* utf8mb4 */
        memset(body + n, 0, 23); n += 23;
        n += (size_t)snprintf(body + n, sizeof(body) - n, "%s", o->user) + 1;
        body[n++] = (char)slen;
        memcpy(body + n, scram, (size_t)slen); n += (size_t)slen;
        n += (size_t)snprintf(body + n, sizeof(body) - n, "%s", o->db) + 1;
        n += (size_t)snprintf(body + n, sizeof(body) - n, "mysql_native_password") + 1;

        if (n + 4 > outcap) return -1;
        out[0] = (char)(n & 0xFF); out[1] = (char)((n >> 8) & 0xFF); out[2] = (char)((n >> 16) & 0xFF);
        out[3] = 1;                                       /* the sequence number */
        memcpy(out + 4, body, n);
        *outlen = n + 4;
        s->stage = 1;
        return 1;
    }

    /* stage 1: OK or ERR */
    *consumed = 4 + pl;
    if (p[0] == 0x00) { s->stage = 2; s->rs = 0; return 0; }
    if (p[0] == 0xFE) { s->stage = 2; s->rs = 0; return 0; }   /* a plugin switch is not supported */
    if (p[0] == 0xFF) {
        tl_diag(c, "mysql: the server rejected the login: %.*s",
                (int)(pl > 9 ? pl - 9 : 0), (const char *)p + 9);
        return -1;
    }
    tl_diag(c, "mysql: an unknown answer to the login (first byte 0x%02x)", p[0]);
    return -1;
}

static int m_build(tl_conn *c, const tl_opts *o, uint64_t seq, char *out, size_t cap) {
    my_st *s = c->ps;
    (void)seq;
    s->rs = 0; s->cols_left = 0; s->eofs = 0;
    size_t q = strlen(o->query);
    size_t n = q + 1;
    if (n + 4 > cap) return -1;
    out[0] = (char)(n & 0xFF); out[1] = (char)((n >> 8) & 0xFF); out[2] = (char)((n >> 16) & 0xFF);
    out[3] = 0;
    out[4] = 0x03;                                        /* COM_QUERY */
    memcpy(out + 5, o->query, q);
    return (int)(n + 4);
}

/* The answer's boundaries: OK/ERR is one packet; otherwise a column count, N descriptions,
 * EOF, rows, EOF. */
static int m_parse(tl_conn *c, const tl_opts *o, const char *in, size_t len, int *is_err) {
    (void)o; (void)c;
    size_t off = 0;
    int st = 0, cols = 0, eofs = 0;
    for (;;) {
        if (off + 4 > len) return 0;
        size_t pl = pktlen(in + off);
        if (off + 4 + pl > len) return 0;
        const unsigned char *p = (const unsigned char *)in + off + 4;
        off += 4 + pl;
        if (st == 0) {
            if (p[0] == 0x00) return (int)off;             /* OK */
            if (p[0] == 0xFF) { *is_err = 1; return (int)off; }
            cols = (int)p[0];                              /* < 251 columns is enough for us */
            st = 1;
            continue;
        }
        if (st == 1) {
            if (--cols <= 0) st = 2;                       /* the column descriptions are over */
            continue;
        }
        if (p[0] == 0xFE && pl < 9) {                      /* EOF */
            if (++eofs == 2) return (int)off;
            continue;
        }
        if (p[0] == 0xFF) { *is_err = 1; return (int)off; }
    }
}

static const tl_proto MY = { "mysql", m_init, m_hs, m_build, m_parse, m_free };
const tl_proto *tl_proto_mysql(void) { return &MY; }
