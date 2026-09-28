/* tlgen — the shared load generator of the bench/suite set.
 *
 * One static binary (musl, linked at 0x10000000) runs BOTH on
 * Linux AND on Talaria: the measuring instrument on both sides of the comparison is ONE,
 * so a difference in the numbers cannot be a difference between clients.
 *
 * The model is an OPEN LOOP: the schedule of sends is set by the target rate and does not
 * depend on whether the server keeps up. Latency is counted from the SCHEDULED
 * MOMENT rather than from the moment of the actual write to the socket, so a server's
 * slowdown is visible in the tail (that is what "no coordinated omission" means).
 */
#ifndef TLGEN_H
#define TLGEN_H

#include <stddef.h>
#include <stdint.h>

#define TL_MAXDEPTH   64
#define TL_RBUF       (1u << 20)
#define TL_WBUF       (1u << 20)

/* ---------------------------------------------------------------- histogram
 * HDR-like: the exponent (a power of two) + TL_HMBITS high bits of the mantissa.
 * The relative error is <= 2^-TL_HMBITS (at 10 bits, under 0.1 %),
 * memory is constant, merging streams is adding cells.
 */
#define TL_HEXP    40            /* up to ~1100 s */
#define TL_HMBITS  10
#define TL_HSUB    (1u << TL_HMBITS)
#define TL_HBUCK   (TL_HEXP * TL_HSUB)

typedef struct {
    uint32_t *b;                 /* TL_HBUCK cells */
    uint64_t  n, sum;
    uint64_t  min, max;
} tl_hist;

void     tl_hist_init(tl_hist *h);
void     tl_hist_free(tl_hist *h);
void     tl_hist_add(tl_hist *h, uint64_t v);
void     tl_hist_merge(tl_hist *dst, const tl_hist *src);
uint64_t tl_hist_pct(const tl_hist *h, double p);   /* p in percent: 99.99 */

/* --------------------------------------------------------------------- options */
typedef struct {
    const char *proto;
    const char *host;
    int         port;
    const char *label;
    const char *out_path;        /* the JSON report */
    const char *samples_path;    /* raw samples (binary), optional */
    const char *stand;           /* talaria | linux | ... — the stand's label */
    const char *tags;            /* arbitrary k=v,k=v for the report */

    int      conns;              /* connections IN TOTAL (split across threads) */
    int      depth;              /* how many requests in flight per connection */
    int      threads;
    int      spin;               /* 1 — poll in a spin without epoll_wait */
    int      poisson;            /* 1 — a Poisson stream instead of a uniform one */
    int      pin_base;           /* -1 — do not pin; otherwise thread t on core pin_base+t */
    double   rate;               /* messages/s IN TOTAL; 0 — a closed loop */
    double   duration;           /* seconds of measurement */
    double   warmup;             /* seconds of warm-up (not counted in the statistics) */
    double   timeout;            /* seconds without an answer -> a connection error */
    uint64_t max_backlog;        /* requests of debt after which the run is marked overload */
    uint64_t max_debt_ns;        /* debt IN TIME after which the run is marked overload */
    /* A ceiling on the number of requests (0 — no ceiling). Needed by preparatory
     * runs: a database must be filled ONCE, while a run by time would repeat
     * a heavy query until the end of the measurement and would never finish. */
    uint64_t count;
    unsigned seed;
    double   tsc_hz;             /* 0 — calibrate it ourselves */

    /* the payload (its meaning depends on the protocol) */
    const char *op;              /* get|set|mixed|ping|... */
    int         keyspace;
    int         valsize;
    int         read_pct;        /* the share of reads in mixed, % */
    const char *path;            /* http: URI */
    const char *query;           /* pg/mysql/chhttp: SQL */
    const char *user, *pass, *db, *coll;
} tl_opts;

/* ---------------------------------------------------------------- connection */
struct tl_conn;
typedef struct tl_conn tl_conn;

typedef struct {
    const char *name;
    int  (*conn_init)(tl_conn *c, const tl_opts *o);
    /* The handshake. in/inlen is what has accumulated from the server (may be 0 bytes on
     * the first call right after connect). Puts the next portion into out.
     * Return: 1 — not finished yet, 0 — ready for the load, <0 — an error.
     * *consumed — how many input bytes were eaten, *outlen — how many to send. */
    int  (*handshake)(tl_conn *c, const tl_opts *o, const char *in, size_t inlen,
                      size_t *consumed, char *out, size_t outcap, size_t *outlen);
    int  (*build)(tl_conn *c, const tl_opts *o, uint64_t seq, char *out, size_t outcap);
    /* Parsing ONE answer from the start of the buffer: >0 — the length (eat that much),
     * 0 — the answer has not arrived in full, <0 — a protocol error.
     * *is_err — 1 if the answer was delivered but carries an application error. */
    int  (*parse)(tl_conn *c, const tl_opts *o, const char *in, size_t len, int *is_err);
    void (*conn_free)(tl_conn *c);
} tl_proto;

struct tl_conn {
    int      fd;
    int      idx;
    int      state;              /* 0 connect, 1 handshake, 2 ready, -1 dead */
    char    *rbuf;  size_t rlen;
    char    *wbuf;  size_t wlen, woff;
    uint64_t due[TL_MAXDEPTH];   /* scheduled (tsc) */
    uint64_t snd[TL_MAXDEPTH];   /* the actual send (tsc) */
    int      ihead, icnt;
    uint64_t sent, done, errs;
    void    *ps;                 /* the protocol's state */
    uint64_t last_rx;            /* the tsc of the last byte — for the timeout */
    /* WHY THE CONNECTION DID NOT COME UP — in words, not through errno.
     *
     * Opening a connection is six different failures (socket, connect,
     * the protocol handshake, EOF from the server, AN ERROR FROM THE SERVER, a deadline), and
     * strerror(errno) distinguishes two of them. Worse: some paths returned -1
     * without touching errno, and the report received the errno of a SUCCESSFUL non-blocking
     * connect — "Operation in progress". That is how a FATAL from PostgreSQL looked in the
     * suite ec2all-20260907-180145, and the analysis went into our network stack twice.
     * The protocol puts the server's error text here, conn_open puts the stage. */
    char     diag[256];
};

/* Record the reason for a failure (the first record wins: it is closer to the cause). */
void tl_diag(tl_conn *c, const char *fmt, ...);

/* THE JOB NUMBER: unique per run_job, the same for all connections of one
 * job. It is needed by protocols whose write key must be unique IN THE COLLECTION
 * between runs (mongo insert): in --serve mode the process lives for the whole stand,
 * and a "once per process" nonce gave every next job the same keys — both
 * sides measured the round trip of a duplicate key failure (65-75 % errors at every point
 * of the suite ec2all-20260905-205134). It is set in run_job BEFORE the connections
 * are initialised, single-threaded; the protocols only read it. */
extern uint64_t tl_job_nonce;

const tl_proto *tl_proto_resp(void);
const tl_proto *tl_proto_http(void);
const tl_proto *tl_proto_pg(void);
const tl_proto *tl_proto_mysql(void);
const tl_proto *tl_proto_mongo(void);

/* a fast xorshift — the schedule and the keys must be reproducible */
static inline uint64_t tl_rnd(uint64_t *s) {
    uint64_t x = *s; x ^= x << 13; x ^= x >> 7; x ^= x << 17; return *s = x;
}

#endif
