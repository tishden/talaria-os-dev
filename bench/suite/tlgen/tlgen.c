/* tlgen — an open-loop load generator. See tlgen.h.
 *
 * One static musl binary is built and runs both on Linux and on
 * Talaria. Everything it does differently on the two systems is nothing:
 * sockets, epoll, clock_gettime and rdtsc (CNTVCT_EL0 on arm64) exist on both.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <math.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#include "tlgen.h"

/* ------------------------------------------------------------------ time */

/* The cycle-ish counter: the TSC on x86; on arm64 the generic timer's virtual count
 * (CNTVCT_EL0, readable from EL0 on Linux and on Talaria; 1.05 GHz on a Graviton3,
 * so the resolution is the TSC's). `isb` orders the read after the preceding
 * instructions, as `lfence` does for rdtsc. The calibration below does not care which. */
static inline uint64_t rdtsc(void) {
#if defined(__aarch64__)
    uint64_t v;
    __asm__ __volatile__("isb\n\tmrs %0, cntvct_el0" : "=r"(v) :: "memory");
    return v;
#else
    unsigned lo, hi;
    __asm__ __volatile__("lfence\n\trdtsc" : "=a"(lo), "=d"(hi) :: "memory");
    return ((uint64_t)hi << 32) | lo;
#endif
}

static double g_tsc_hz = 0;

static inline uint64_t ns_of(uint64_t ticks) {
    return (uint64_t)((double)ticks * 1e9 / g_tsc_hz);
}
static inline uint64_t ticks_of_ns(double ns) {
    return (uint64_t)(ns * g_tsc_hz / 1e9);
}
static uint64_t mono_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

/* Calibrating the TSC by busy waiting: without nanosleep (during warm-up this is the only
 * place where we need the system clock at all) and without depending on what
 * the hypervisor told the guest. */
static double calibrate_tsc(double ms) {
    uint64_t n0 = mono_ns(), t0 = rdtsc(), n1, t1;
    uint64_t want = (uint64_t)(ms * 1e6);
    do { n1 = mono_ns(); t1 = rdtsc(); } while (n1 - n0 < want);
    return (double)(t1 - t0) * 1e9 / (double)(n1 - n0);
}

/* ------------------------------------------------------------- histogram */

void tl_hist_init(tl_hist *h) {
    h->b = calloc(TL_HBUCK, sizeof(uint32_t));
    h->n = h->sum = h->max = 0;
    h->min = UINT64_MAX;
}
void tl_hist_free(tl_hist *h) { free(h->b); h->b = NULL; }

static inline int hbin(uint64_t v) {
    if (v < TL_HSUB) return (int)v;                 /* the linear zone: 0..1023 ns */
    int e = 63 - __builtin_clzll(v);                /* a power of two */
    if (e >= TL_HEXP) e = TL_HEXP - 1;
    int m = (int)((v >> (e - TL_HMBITS)) & (TL_HSUB - 1));
    return e * TL_HSUB + m;
}
/* The bucket's lower bound — that is what we report a percentile as (conservatively). */
static inline uint64_t hval(int i) {
    if (i < (int)TL_HSUB) return (uint64_t)i;
    int e = i / TL_HSUB, m = i % TL_HSUB;
    return ((uint64_t)m | TL_HSUB) << (e - TL_HMBITS);
}

void tl_hist_add(tl_hist *h, uint64_t v) {
    h->b[hbin(v)]++;
    h->n++; h->sum += v;
    if (v < h->min) h->min = v;
    if (v > h->max) h->max = v;
}
void tl_hist_merge(tl_hist *d, const tl_hist *s) {
    if (!s->n) return;
    for (int i = 0; i < (int)TL_HBUCK; i++) d->b[i] += s->b[i];
    d->n += s->n; d->sum += s->sum;
    if (s->min < d->min) d->min = s->min;
    if (s->max > d->max) d->max = s->max;
}
uint64_t tl_hist_pct(const tl_hist *h, double p) {
    if (!h->n) return 0;
    /* "no less than p % of the samples" — the usual HdrHistogram convention */
    uint64_t want = (uint64_t)(p / 100.0 * (double)h->n + 0.5);
    if (want == 0) want = 1;
    if (want > h->n) want = h->n;
    uint64_t acc = 0;
    for (int i = 0; i < (int)TL_HBUCK; i++) {
        acc += h->b[i];
        if (acc >= want) {
            uint64_t v = hval(i);
            return v > h->max ? h->max : v;
        }
    }
    return h->max;
}

/* --------------------------------------------------------------- worker */

#define MAXSEC 3600

typedef struct {
    uint64_t n, errs;
} tl_sec;

typedef struct {
    int             id;
    const tl_opts  *o;
    const tl_proto *P;
    tl_conn        *cs;
    int             nconn;
    double          rate;                 /* this thread's share */
    uint64_t        count;                /* this thread's request ceiling (0 — none) */
    uint64_t        t0, twarm, tend;      /* tsc boundaries */
    struct sockaddr_in sa;
    int             ep;

    tl_hist  lat, svc;
    tl_hist *sech;                        /* one histogram per second of the measurement */
    tl_sec  *secn;
    int      nsec;

    uint64_t sent, done, errs, timeouts, connerrs, apperrs;
    uint64_t backlog_max, backlog_ns, sched_total;
    uint64_t n_send, n_recv, n_recv0, n_wait; /* syscalls, counted by the generator itself */
    int      overload;

    uint32_t *smp;  size_t smp_n, smp_cap;
    uint64_t min_svc, floor_t;            /* the polling floor: see worker_main */
    uint64_t reopen_at;                   /* do not try to reconnect before this tsc */

    uint64_t seed;
    /* The failure line carries THE SERVER'S TEXT (`tl_conn.diag`, up to 256 bytes) plus
     * the address and the connection number — 256 bytes cut it exactly at the most important part. */
    char     err[512];
} tl_worker;

static volatile int g_stop = 0;
static void on_sig(int s) { (void)s; g_stop = 1; }

/* The reason a connection failed — in words. The first record wins: it is set by
 * whoever is closest to the cause (the protocol that saw the server's error), while
 * callers higher up the stack only refine the stage. */
void tl_diag(tl_conn *c, const char *fmt, ...) {
    if (!c || c->diag[0]) return;
    va_list ap; va_start(ap, fmt);
    vsnprintf(c->diag, sizeof(c->diag), fmt, ap);
    va_end(ap);
}

static int set_nb(int fd) {
    int fl = fcntl(fd, F_GETFL, 0);
    return fcntl(fd, F_SETFL, fl | O_NONBLOCK);
}
static int set_block(int fd) {
    int fl = fcntl(fd, F_GETFL, 0);
    return fcntl(fd, F_SETFL, fl & ~O_NONBLOCK);
}
/* connect WITH A TIME CEILING. A blocking connect to a silent server is
 * up to 10 s on Talaria and up to two minutes on Linux, and all that time the
 * generator does not send on schedule: one timeout in the middle of a run turned
 * into seconds of debt, and the point into "overload" without overload. */
static int connect_bounded(int fd, const struct sockaddr *sa, socklen_t len, int ms) {
    if (set_nb(fd) < 0) return -1;
    int r = connect(fd, sa, len);
    if (r < 0 && errno != EINPROGRESS) return -1;
    if (r < 0) {
        struct pollfd p = { .fd = fd, .events = POLLOUT };
        if (poll(&p, 1, ms) <= 0) { errno = ETIMEDOUT; return -1; }
        int err = 0; socklen_t el = sizeof(err);
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &el) < 0) return -1;
        if (err) { errno = err; return -1; }
    }
    return set_block(fd);
}

/* Open a connection and carry the handshake through. Synchronously and with
 * a blocking socket: connecting is preparing the stand, not measuring, and
 * an error here must be seen at once rather than dissolved in the tail of the distribution. */
static int conn_open(tl_worker *w, tl_conn *c) {
    const tl_opts *o = w->o;
    if (c->fd >= 0) { close(c->fd); c->fd = -1; }
    c->rlen = c->wlen = c->woff = 0;
    c->ihead = c->icnt = 0;
    c->state = 0;

    c->diag[0] = 0;
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { tl_diag(c, "socket(): %s", strerror(errno)); return -1; }
    /* The ceiling on the whole opening (connect + the protocol handshake) is the
     * request timeout: no point has to wait for the server longer than that. */
    int open_ms = (int)(o->timeout * 1000);
    if (open_ms < 100) open_ms = 100;
    uint64_t deadline = rdtsc() + ticks_of_ns(o->timeout * 1e9);
    if (connect_bounded(fd, (struct sockaddr *)&w->sa, sizeof(w->sa), open_ms) < 0) {
        tl_diag(c, "connect(): %s (ceiling %d ms)", strerror(errno), open_ms);
        close(fd); return -1;
    }
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
    c->fd = fd;

    if (w->P->conn_init && w->P->conn_init(c, o) < 0) {
        tl_diag(c, "%s: connection initialisation failed", w->P->name);
        close(fd); c->fd = -1; return -1;
    }

    /* the handshake — with a blocking socket, with the same time ceiling */
    char out[8192];
    size_t consumed = 0, outlen = 0;
    for (;;) {
        int r = w->P->handshake(c, o, c->rbuf, c->rlen, &consumed, out, sizeof(out), &outlen);
        /* The server's error text is put in by the protocol itself (`tl_diag`) — here there is only
         * a fallback line in case it did not do so. */
        if (r < 0) { tl_diag(c, "%s: the handshake was rejected by the server", w->P->name); close(fd); c->fd = -1; return -1; }
        if (consumed) { memmove(c->rbuf, c->rbuf + consumed, c->rlen - consumed); c->rlen -= consumed; consumed = 0; }
        if (outlen) {
            size_t off = 0;
            while (off < outlen) {
                ssize_t k = send(fd, out + off, outlen - off, 0);
                if (k <= 0) {
                    tl_diag(c, "%s: handshake, send returned %zd (%s)", w->P->name, k, k ? strerror(errno) : "the connection was closed");
                    close(fd); c->fd = -1; return -1;
                }
                off += (size_t)k;
            }
            outlen = 0;
        }
        if (r == 0) break;
        /* Waiting for an answer — no longer than the remaining ceiling: otherwise a blocking recv on a
         * silent server stands until the server closes the connection itself. */
        uint64_t nowt = rdtsc();
        if (nowt > deadline) { tl_diag(c, "%s: the handshake did not fit into %.3f s", w->P->name, o->timeout); close(fd); c->fd = -1; errno = ETIMEDOUT; return -1; }
        struct pollfd p = { .fd = fd, .events = POLLIN };
        int left_ms = (int)(ns_of(deadline - nowt) / 1000000ull) + 1;
        if (poll(&p, 1, left_ms) <= 0) { tl_diag(c, "%s: the server is silent during the handshake for %.3f s", w->P->name, o->timeout); close(fd); c->fd = -1; errno = ETIMEDOUT; return -1; }
        ssize_t k = recv(fd, c->rbuf + c->rlen, TL_RBUF - c->rlen, 0);
        if (k <= 0) {
            /* k == 0 means the server CLOSED the connection in the middle of the handshake. That is not
             * a network error: that is how a server answers when it accepted TCP but
             * would not serve (it ran out of worker processes, a limit refusal).
             * This used to carry the errno from connect. */
            tl_diag(c, "%s: the server closed the connection during the handshake (%s), %zu B received",
                    w->P->name, k ? strerror(errno) : "EOF", c->rlen);
            close(fd); c->fd = -1; return -1;
        }
        c->rlen += (size_t)k;
    }

    set_nb(fd);
    c->state = 2;
    c->last_rx = rdtsc();
    if (w->ep >= 0) {
        struct epoll_event ev = {0};
        ev.events = EPOLLIN; ev.data.ptr = c;
        epoll_ctl(w->ep, EPOLL_CTL_ADD, fd, &ev);
    }
    return 0;
}

static void record(tl_worker *w, uint64_t now, uint64_t due, uint64_t snd, int is_err) {
    w->done++;
    if (is_err) w->apperrs++;
    if (now < w->twarm) return;
    uint64_t lat = now > due ? ns_of(now - due) : 0;
    uint64_t svc = now > snd ? ns_of(now - snd) : 0;
    /* The polling floor: an answer physically cannot arrive faster than the observed minimum,
     * so up to 3/4 of that minimum recv need not be called at all. That removes
     * dozens of idle syscalls per request while delaying nothing: the threshold is always
     * STRICTLY below the minimum, and it is only ever updated downwards. */
    uint64_t sv_t = now > snd ? now - snd : 0;
    if (sv_t && sv_t < w->min_svc) { w->min_svc = sv_t; w->floor_t = sv_t * 3 / 4; }
    tl_hist_add(&w->lat, lat);
    tl_hist_add(&w->svc, svc);
    int s = (int)(ns_of(now - w->twarm) / 1000000000ull);
    if (s >= 0 && s < w->nsec) {
        tl_hist_add(&w->sech[s], lat);
        w->secn[s].n++;
        if (is_err) w->secn[s].errs++;
    }
    if (w->smp && w->smp_n + 3 <= w->smp_cap) {
        w->smp[w->smp_n++] = (uint32_t)(ns_of(now - w->twarm) / 1000000ull);
        w->smp[w->smp_n++] = (uint32_t)(lat > 0xFFFFFFFFull ? 0xFFFFFFFFull : lat);
        w->smp[w->smp_n++] = (uint32_t)(svc > 0xFFFFFFFFull ? 0xFFFFFFFFull : svc);
    }
}

/* Send the rest of wbuf. A return of <0 means the connection died. */
static int flush_conn(tl_worker *w, tl_conn *c) {
    while (c->woff < c->wlen) {
        w->n_send++;
        ssize_t k = send(c->fd, c->wbuf + c->woff, c->wlen - c->woff, MSG_NOSIGNAL);
        if (k > 0) { c->woff += (size_t)k; continue; }
        if (k < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
        return -1;
    }
    c->wlen = c->woff = 0;
    return 0;
}

static int drain_conn(tl_worker *w, tl_conn *c) {
    for (;;) {
        if (c->rlen == TL_RBUF) return -1;              /* the answer did not fit — an assembly error */
        ssize_t k = recv(c->fd, c->rbuf + c->rlen, TL_RBUF - c->rlen, 0);
        if (k > 0) {
            w->n_recv++;
            c->rlen += (size_t)k;
            uint64_t now = rdtsc();
            c->last_rx = now;
            size_t off = 0;
            for (;;) {
                int is_err = 0;
                int n = w->P->parse(c, w->o, c->rbuf + off, c->rlen - off, &is_err);
                if (n == 0) break;
                if (n < 0) return -1;
                off += (size_t)n;
                if (c->icnt > 0) {
                    record(w, now, c->due[c->ihead], c->snd[c->ihead], is_err);
                    c->ihead = (c->ihead + 1) % TL_MAXDEPTH;
                    c->icnt--;
                }
            }
            if (off) { memmove(c->rbuf, c->rbuf + off, c->rlen - off); c->rlen -= off; }
            if ((size_t)k < TL_RBUF / 4) return 0;      /* the socket is most likely empty */
            continue;
        }
        if (k == 0) return -1;
        if (errno == EAGAIN || errno == EWOULDBLOCK) { w->n_recv0++; return 0; }
        if (errno == EINTR) continue;
        return -1;
    }
}

static void *worker_main(void *arg) {
    tl_worker *w = arg;
    const tl_opts *o = w->o;

    if (o->pin_base >= 0) {
        cpu_set_t st; CPU_ZERO(&st); CPU_SET(o->pin_base + w->id, &st);
        sched_setaffinity(0, sizeof(st), &st);
    }

    for (int i = 0; i < w->nconn; i++) {
        if (conn_open(w, &w->cs[i]) < 0) {
            snprintf(w->err, sizeof(w->err), "thread %d: connection %d to %s:%d did not come up: %s",
                     w->id, i, o->host, o->port,
                     w->cs[i].diag[0] ? w->cs[i].diag : strerror(errno));
            /* Close the ALREADY OPEN connections HERE. The cleanup at the end of
             * `worker_main` is not reached through this exit, while a process in
             * `--serve` mode lives for the whole stand: every failed job left
             * up to nconn-1 hanging sockets — and the server's worker processes hung on them.
             * A failure of one point thus broke all the following ones. */
            for (int j = 0; j < i; j++)
                if (w->cs[j].fd >= 0) { close(w->cs[j].fd); w->cs[j].fd = -1; }
            return NULL;
        }
    }

    uint64_t iv = w->rate > 0 ? (uint64_t)(g_tsc_hz / w->rate) : 0;
    uint64_t next_due = w->t0;
    uint64_t seq = 0;
    int rr = 0;
    struct epoll_event evs[256];
    char req[65536];
    uint64_t tmo = ticks_of_ns(o->timeout * 1e9);

    while (!g_stop) {
        uint64_t now = rdtsc();
        int gen_done = (now >= w->tend) || (w->count && w->sent >= w->count);

        /* 1. send everything that is due */
        if (!gen_done) {
            int guard = 0;
            while (next_due <= now && guard++ < 4096) {
                tl_conn *c = NULL;
                for (int k = 0; k < w->nconn; k++) {
                    tl_conn *t = &w->cs[(rr + k) % w->nconn];
                    if (t->state == 2 && t->icnt < o->depth && t->wlen < TL_WBUF - 4096) { c = t; rr = (rr + k + 1) % w->nconn; break; }
                }
                if (!c) break;                          /* debt: we do not move the schedule */
                int n = w->P->build(c, o, seq, req, sizeof(req));
                if (n <= 0) { snprintf(w->err, sizeof(w->err), "the request could not be assembled"); g_stop = 1; break; }
                memcpy(c->wbuf + c->wlen, req, (size_t)n);
                c->wlen += (size_t)n;
                int t = (c->ihead + c->icnt) % TL_MAXDEPTH;
                c->due[t] = next_due;
                c->snd[t] = rdtsc();
                c->icnt++;
                c->sent++; w->sent++; seq++; w->sched_total++;
                if (flush_conn(w, c) < 0) { c->state = -1; }
                if (w->rate > 0) {
                    if (o->poisson) {
                        double u = (double)(tl_rnd(&w->seed) >> 11) / 9007199254740992.0;
                        if (u < 1e-12) u = 1e-12;
                        next_due += (uint64_t)(-log(u) * (double)iv);
                    } else next_due += iv;
                } else next_due = now;                  /* a closed loop */
            }
            /* The size of the debt — how far the schedule has run ahead of what was sent.
             *
             * The debt is counted both in requests and IN TIME, and the decision is made by
             * time: debt in requests depends on the rate, while how spoiled the measurement is
             * does not. Every request sent with a debt of D gets D added
             * to its latency; so when D is comparable with the latency itself, the
             * number speaks about the generator.
             *
             * ONLY AFTER THE WARM-UP: at the start the debt accumulates from setting up
             * connections and filling the pool, and before this fix it stayed in
             * `backlog_max` forever — a point with a debt of 722 requests looked
             * suspicious even though its open loop differed from the service one by
             * 0.4 ms. */
            if (w->rate > 0 && now > next_due && now >= w->twarm) {
                uint64_t debt = (now - next_due) / (iv ? iv : 1);
                uint64_t debt_ns = ns_of(now - next_due);
                if (debt > w->backlog_max) w->backlog_max = debt;
                if (debt_ns > w->backlog_ns) w->backlog_ns = debt_ns;
                if (debt > o->max_backlog) w->overload = 1;
                if (o->max_debt_ns && debt_ns > o->max_debt_ns) w->overload = 1;
            }
        }

        /* 2. send the remaining tails */
        for (int i = 0; i < w->nconn; i++) {
            tl_conn *c = &w->cs[i];
            if (c->state == 2 && c->wlen > c->woff && flush_conn(w, c) < 0) c->state = -1;
        }

        /* 3. collect the answers */
        /* How many milliseconds we may sleep without being late for the next deadline. */
        int to = 0;
        if (gen_done || w->rate <= 0) to = 0;
        else {
            uint64_t nn = rdtsc();
            to = next_due > nn ? (int)(ns_of(next_due - nn) / 1000000ull) : 0;
            if (to > 10) to = 10;
        }
        /* to == 0 means "the deadline is less than a millisecond away". epoll_wait with a zero
         * timeout is the same busy polling, only a syscall more expensive, so
         * with a dense schedule we poll the sockets directly. Hence the
         * rule: an exact layout by microsecond-scale deadlines
         * REQUIRES busy waiting, and the --spin mode calls it
         * by its name. */
        if (o->spin || to == 0) {
            for (int i = 0; i < w->nconn; i++) {
                tl_conn *c = &w->cs[i];
                if (c->state != 2 || !c->icnt) continue;
                if (w->floor_t && rdtsc() - c->snd[c->ihead] < w->floor_t) continue;
                if (drain_conn(w, c) < 0) c->state = -1;
            }
        } else {
            w->n_wait++;
            int nev = epoll_wait(w->ep, evs, 256, to);
            for (int i = 0; i < nev; i++) {
                tl_conn *c = evs[i].data.ptr;
                if (c->state == 2 && drain_conn(w, c) < 0) c->state = -1;
            }
        }

        /* 4. timeouts and reconnection */
        now = rdtsc();
        for (int i = 0; i < w->nconn; i++) {
            tl_conn *c = &w->cs[i];
            if (c->state == 2 && c->icnt > 0 && now - c->due[c->ihead] > tmo) {
                w->timeouts += (uint64_t)c->icnt;
                c->state = -1;
            }
            if (c->state == -1) {
                w->connerrs++;
                if (c->icnt) { w->errs += (uint64_t)c->icnt; c->icnt = 0; }
                if (w->ep >= 0 && c->fd >= 0) epoll_ctl(w->ep, EPOLL_CTL_DEL, c->fd, NULL);
                /* We reconnect NOT IMMEDIATELY. Without a pause the very first broken
                 * connection turns into a storm: the generator hammers the server with thousands of
                 * connects per second, eats up the ephemeral ports and TIME_WAIT — and
                 * the FOLLOWING runs cannot connect either. One breakage
                 * then looks like a wall of refusals, and the cause is lost. */
                if (!gen_done && now >= w->reopen_at) {
                    if (conn_open(w, c) < 0) {
                        c->state = -1;
                        w->reopen_at = now + ticks_of_ns(50e6);   /* 50 ms */
                    }
                }
            }
        }

        /* 5. the end: generation is over and everything in flight has either returned or expired */
        if (gen_done) {
            int busy = 0;
            for (int i = 0; i < w->nconn; i++) if (w->cs[i].state == 2 && w->cs[i].icnt) busy = 1;
            if (!busy) break;
            if (rdtsc() > w->tend + tmo) break;
        }
    }
    for (int i = 0; i < w->nconn; i++) if (w->cs[i].fd >= 0) close(w->cs[i].fd);
    return NULL;
}

/* ---------------------------------------------------------------- the report */

static const double PCTS[] = {50, 75, 90, 99, 99.9, 99.99, 99.999};
static const char  *PNAM[] = {"p50", "p75", "p90", "p99", "p99_9", "p99_99", "p99_999"};

/* A string in JSON: quotes, backslashes and control characters are escaped. */
static void json_str(FILE *f, const char *p) {
    for (; *p; p++) {
        unsigned char ch = (unsigned char)*p;
        if (ch == '"' || ch == '\\') { fputc('\\', f); fputc(ch, f); }
        else if (ch < 0x20) fprintf(f, "\\u%04x", ch);
        else fputc(ch, f);
    }
}
static int g_job_wrote_json;   /* run_job wrote the report (the receiver does not duplicate it by code) */

static void json_hist(FILE *f, const char *name, const tl_hist *h) {
    fprintf(f, "    \"%s\": {\"n\": %llu, \"min_ns\": %llu, \"mean_ns\": %.1f, \"max_ns\": %llu",
            name, (unsigned long long)h->n,
            (unsigned long long)(h->n ? h->min : 0),
            h->n ? (double)h->sum / (double)h->n : 0.0,
            (unsigned long long)h->max);
    for (size_t i = 0; i < sizeof(PCTS) / sizeof(PCTS[0]); i++)
        fprintf(f, ", \"%s\": %llu", PNAM[i], (unsigned long long)tl_hist_pct(h, PCTS[i]));
    fprintf(f, "}");
}

/* One measurement: argument parsing, the run, the report. Moved out of main because
 * in --serve mode there are many such runs over the process's life (see serve_loop). */
uint64_t tl_job_nonce;
static int run_job(int argc, char **argv, FILE *sock_out) {
    tl_opts o;
    {   /* the job's nonce — see tlgen.h */
        struct timespec ts;
        clock_gettime(CLOCK_REALTIME, &ts);
        tl_job_nonce = (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
    }
    memset(&o, 0, sizeof(o));
    o.proto = "resp"; o.host = "127.0.0.1"; o.port = 0;
    o.conns = 1; o.depth = 1; o.threads = 1; o.rate = 0;
    o.duration = 10; o.warmup = 2; o.timeout = 5; o.max_backlog = 0; o.max_debt_ns = 0;
    o.seed = 12345; o.pin_base = -1; o.label = "run"; o.stand = "unknown";
    o.op = "ping"; o.keyspace = 100000; o.valsize = 64; o.read_pct = 100;
    o.path = "/"; o.query = "SELECT 1"; o.user = "talaria"; o.pass = "";
    o.db = "talaria"; o.coll = "bench"; o.tags = "";

    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        #define ARG(n) (!strcmp(a, "--" n) && i + 1 < argc)
        if      (ARG("proto"))    o.proto = argv[++i];
        else if (ARG("host"))     o.host = argv[++i];
        else if (ARG("port"))     o.port = atoi(argv[++i]);
        else if (ARG("conns"))    o.conns = atoi(argv[++i]);
        else if (ARG("depth"))    o.depth = atoi(argv[++i]);
        else if (ARG("threads"))  o.threads = atoi(argv[++i]);
        else if (ARG("rate"))     o.rate = atof(argv[++i]);
        else if (ARG("duration")) o.duration = atof(argv[++i]);
        else if (ARG("warmup"))   o.warmup = atof(argv[++i]);
        else if (ARG("timeout"))  o.timeout = atof(argv[++i]);
        else if (ARG("seed"))     o.seed = (unsigned)atoi(argv[++i]);
        else if (ARG("pin"))      o.pin_base = atoi(argv[++i]);
        else if (ARG("out"))      o.out_path = argv[++i];
        else if (ARG("samples"))  o.samples_path = argv[++i];
        else if (ARG("name"))     o.label = argv[++i];
        else if (ARG("stand"))    o.stand = argv[++i];
        else if (ARG("tags"))     o.tags = argv[++i];
        else if (ARG("op"))       o.op = argv[++i];
        else if (ARG("keyspace")) o.keyspace = atoi(argv[++i]);
        else if (ARG("valsize"))  o.valsize = atoi(argv[++i]);
        else if (ARG("read-pct")) o.read_pct = atoi(argv[++i]);
        else if (ARG("path"))     o.path = argv[++i];
        else if (ARG("query"))    o.query = argv[++i];
        else if (ARG("user"))     o.user = argv[++i];
        else if (ARG("pass"))     o.pass = argv[++i];
        else if (ARG("db"))       o.db = argv[++i];
        else if (ARG("coll"))     o.coll = argv[++i];
        else if (ARG("tsc-hz"))   o.tsc_hz = atof(argv[++i]);
        else if (ARG("max-backlog")) o.max_backlog = (uint64_t)atoll(argv[++i]);
        else if (ARG("max-debt-ms")) o.max_debt_ns = (uint64_t)(atof(argv[++i]) * 1e6);
        else if (ARG("count"))    o.count = (uint64_t)atoll(argv[++i]);
        else if (ARG("serve"))    { i++; }   /* parsed in main */
        else if (!strcmp(a, "--spin"))    o.spin = 1;
        else if (!strcmp(a, "--epoll"))   o.spin = 0;
        else if (!strcmp(a, "--poisson")) o.poisson = 1;
        else if (!strcmp(a, "--help")) {
            printf("tlgen --proto resp|http|pg|mysql|mongo --host H --port P "
                   "[--conns N --depth D --threads T --rate R --duration S --warmup S]\n"
                   "      [--spin|--epoll] [--poisson] [--pin CPU0] [--op ...] [--out file.json]\n"
                   "      [--max-backlog N] [--max-debt-ms MS]   the generator's debt after\n"
                   "         which a run is marked overload (5 ms by default)\n");
            return 0;
        }
        else { fprintf(stderr, "tlgen: unclear argument: %s\n", a); return 2; }
        #undef ARG
    }

    const tl_proto *P;
    if      (!strcmp(o.proto, "resp"))  { P = tl_proto_resp();  if (!o.port) o.port = 6379; }
    else if (!strcmp(o.proto, "http"))  { P = tl_proto_http();  if (!o.port) o.port = 80; }
    else if (!strcmp(o.proto, "chhttp")){ P = tl_proto_http();  if (!o.port) o.port = 8123; }
    else if (!strcmp(o.proto, "pg"))    { P = tl_proto_pg();    if (!o.port) o.port = 5432; }
    else if (!strcmp(o.proto, "mysql")) { P = tl_proto_mysql(); if (!o.port) o.port = 3306; }
    else if (!strcmp(o.proto, "mongo")) { P = tl_proto_mongo(); if (!o.port) o.port = 27017; }
    else { fprintf(stderr, "tlgen: unknown protocol: %s\n", o.proto); return 2; }

    if (o.depth > TL_MAXDEPTH) { fprintf(stderr, "tlgen: --depth is larger than %d\n", TL_MAXDEPTH); return 2; }
    if (o.conns < o.threads) o.conns = o.threads;
    /* Half a second of debt is no longer "jitter" but "the server does not hold the requested rate".
     * The default threshold is exactly that: a run with this flag goes into the report, but
     * its percentiles measure the generator's queue rather than the server. */
    if (!o.max_backlog) o.max_backlog = (uint64_t)(o.rate > 0 ? o.rate * 0.5 : 1e18);
    /* Debt IN TIME is always REPORTED (`backlog_ns`), but by default it does not
     * mark a run overloaded — and that came out of a measurement. A threshold of five
     * milliseconds caught a SINGLE debt spike somewhere in the middle of a run and
     * discarded the point ENTIRELY, together with healthy p50 and p99: on this suite
     * half the comparable points were lost that way. How spoiled a run is is judged more precisely and
     * PER PERCENTILE on the report's side (`lib/report.py`, `queued_at`): there, for
     * each percentile, one can see how far the open loop diverged from the service one,
     * i.e. exactly which part of the distribution the debt spoiled.
     * `--max-debt-ms N` turns the hard flag on when it is explicitly wanted. */

    g_stop = 0;
    g_tsc_hz = o.tsc_hz > 0 ? o.tsc_hz : calibrate_tsc(200);
    fprintf(stderr, "tlgen: proto=%s target=%s:%d conns=%d depth=%d threads=%d "
                    "rate=%.0f/s measure=%.0fs warmup=%.0fs poll=%s TSC=%.3f GHz\n",
            o.proto, o.host, o.port, o.conns, o.depth, o.threads, o.rate,
            o.duration, o.warmup, o.spin ? "spin" : "epoll", g_tsc_hz / 1e9);

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET; sa.sin_port = htons((uint16_t)o.port);
    if (inet_pton(AF_INET, o.host, &sa.sin_addr) != 1) { fprintf(stderr, "tlgen: --host must be IPv4: %s\n", o.host); return 2; }

    int nsec = (int)(o.duration + 1.5);
    if (nsec > MAXSEC) nsec = MAXSEC;

    tl_worker *ws = calloc((size_t)o.threads, sizeof(tl_worker));
    uint64_t t0 = rdtsc() + ticks_of_ns(50e6);       /* a common start in 50 ms */
    size_t smp_cap = o.samples_path ? (size_t)3 * 8000000 / (size_t)o.threads : 0;

    for (int t = 0; t < o.threads; t++) {
        tl_worker *w = &ws[t];
        w->id = t; w->o = &o; w->P = P; w->sa = sa;
        w->nconn = o.conns / o.threads + (t < o.conns % o.threads ? 1 : 0);
        w->rate = o.rate > 0 ? o.rate / o.threads : 0;
        w->count = o.count ? (o.count + (uint64_t)o.threads - 1) / (uint64_t)o.threads : 0;
        w->t0 = t0;
        w->twarm = t0 + ticks_of_ns(o.warmup * 1e9);
        w->tend = w->twarm + ticks_of_ns(o.duration * 1e9);
        w->seed = o.seed * 2654435761u + (uint64_t)t * 40503u + 1;
        w->ep = o.spin ? -1 : epoll_create1(0);
        tl_hist_init(&w->lat); tl_hist_init(&w->svc);
        w->nsec = nsec;
        w->sech = calloc((size_t)nsec, sizeof(tl_hist));
        w->secn = calloc((size_t)nsec, sizeof(tl_sec));
        for (int s = 0; s < nsec; s++) tl_hist_init(&w->sech[s]);
        w->cs = calloc((size_t)w->nconn, sizeof(tl_conn));
        for (int i = 0; i < w->nconn; i++) {
            w->cs[i].fd = -1; w->cs[i].idx = i;
            w->cs[i].rbuf = malloc(TL_RBUF);
            w->cs[i].wbuf = malloc(TL_WBUF);
        }
        if (smp_cap) { w->smp = malloc(smp_cap * sizeof(uint32_t)); w->smp_cap = smp_cap; }
    }

    uint64_t wall0 = mono_ns();
    pthread_t *th = calloc((size_t)o.threads, sizeof(pthread_t));
    for (int t = 1; t < o.threads; t++) pthread_create(&th[t], NULL, worker_main, &ws[t]);
    worker_main(&ws[0]);
    for (int t = 1; t < o.threads; t++) pthread_join(th[t], NULL);
    uint64_t wall1 = mono_ns();

    /* --- reduction --- */
    tl_hist lat, svc; tl_hist_init(&lat); tl_hist_init(&svc);
    uint64_t sent = 0, done = 0, errs = 0, tmo = 0, cerr = 0, aerr = 0, blmax = 0, blns = 0;
    uint64_t nsend = 0, nrecv = 0, nrecv0 = 0, nwait = 0;
    int overload = 0;
    const char *firsterr = NULL;
    for (int t = 0; t < o.threads; t++) {
        tl_hist_merge(&lat, &ws[t].lat); tl_hist_merge(&svc, &ws[t].svc);
        sent += ws[t].sent; done += ws[t].done; errs += ws[t].errs;
        tmo += ws[t].timeouts; cerr += ws[t].connerrs; aerr += ws[t].apperrs;
        nsend += ws[t].n_send; nrecv += ws[t].n_recv; nrecv0 += ws[t].n_recv0; nwait += ws[t].n_wait;
        if (ws[t].backlog_max > blmax) blmax = ws[t].backlog_max;
        if (ws[t].backlog_ns > blns) blns = ws[t].backlog_ns;
        overload |= ws[t].overload;
        if (!firsterr && ws[t].err[0]) firsterr = ws[t].err;
    }
    if (firsterr) fprintf(stderr, "tlgen: ERROR: %s\n", firsterr);

    double secs = (double)(wall1 - wall0) / 1e9 - o.warmup;
    if (secs <= 0) secs = 1e-9;
    double thr = (double)lat.n / secs;

    FILE *f = o.out_path ? fopen(o.out_path, "w") : (sock_out ? sock_out : stdout);
    if (!f) { fprintf(stderr, "tlgen: could not open %s\n", o.out_path); return 2; }
    fprintf(f, "{\n  \"schema\": 1,\n  \"name\": \"%s\",\n  \"stand\": \"%s\",\n"
               "  \"proto\": \"%s\",\n  \"op\": \"%s\",\n  \"target\": \"%s:%d\",\n"
               "  \"tags\": \"%s\",\n  \"wall_s\": %.3f,\n  \"tsc_hz\": %.0f,\n",
            o.label, o.stand, o.proto, o.op, o.host, o.port, o.tags,
            (double)(wall1 - wall0) / 1e9, g_tsc_hz);
    fprintf(f, "  \"cfg\": {\"conns\": %d, \"depth\": %d, \"threads\": %d, \"rate\": %.0f,"
               " \"duration\": %.1f, \"warmup\": %.1f, \"count\": %llu, \"poll\": \"%s\", \"dist\": \"%s\","
               " \"keyspace\": %d, \"valsize\": %d, \"read_pct\": %d, \"seed\": %u},\n",
            o.conns, o.depth, o.threads, o.rate, o.duration, o.warmup,
            (unsigned long long)o.count,
            o.spin ? "spin" : "epoll", o.poisson ? "poisson" : "uniform",
            o.keyspace, o.valsize, o.read_pct, o.seed);
    fprintf(f, "  \"counts\": {\"sent\": %llu, \"done\": %llu, \"measured\": %llu,"
               " \"lost\": %llu, \"timeouts\": %llu, \"conn_errors\": %llu, \"app_errors\": %llu,"
               " \"backlog_max\": %llu, \"backlog_ns\": %llu, \"overload\": %s},\n",
            (unsigned long long)sent, (unsigned long long)done, (unsigned long long)lat.n,
            (unsigned long long)errs, (unsigned long long)tmo, (unsigned long long)cerr,
            (unsigned long long)aerr, (unsigned long long)blmax, (unsigned long long)blns,
            overload ? "true" : "false");
    double achieved = o.rate > 0 ? 100.0 * thr / o.rate : 100.0;
    fprintf(f, "  \"throughput_rps\": %.1f,\n  \"rate_achieved_pct\": %.2f,\n", thr, achieved);
    fprintf(f, "  \"syscalls\": {\"send\": %llu, \"recv_data\": %llu, \"recv_empty\": %llu,"
               " \"epoll_wait\": %llu, \"useful_per_req\": %.3f, \"all_per_req\": %.1f},\n",
            (unsigned long long)nsend, (unsigned long long)nrecv, (unsigned long long)nrecv0,
            (unsigned long long)nwait,
            done ? (double)(nsend + nrecv) / (double)done : 0.0,
            done ? (double)(nsend + nrecv + nrecv0 + nwait) / (double)done : 0.0);
    fprintf(f, "  \"latency\": {\n");
    json_hist(f, "open_loop", &lat); fprintf(f, ",\n");
    json_hist(f, "service", &svc);   fprintf(f, "\n  },\n");

    /* the timeline: for every second of the measurement — the rate and the tail */
    fprintf(f, "  \"timeline\": [");
    for (int s = 0; s < nsec; s++) {
        tl_hist h; tl_hist_init(&h);
        uint64_t n = 0, e = 0;
        for (int t = 0; t < o.threads; t++) { tl_hist_merge(&h, &ws[t].sech[s]); n += ws[t].secn[s].n; e += ws[t].secn[s].errs; }
        if (n) fprintf(f, "%s\n    {\"s\": %d, \"n\": %llu, \"errs\": %llu, \"p50\": %llu, \"p99\": %llu, \"p99_9\": %llu, \"max\": %llu}",
                       s ? "," : "", s, (unsigned long long)n, (unsigned long long)e,
                       (unsigned long long)tl_hist_pct(&h, 50), (unsigned long long)tl_hist_pct(&h, 99),
                       (unsigned long long)tl_hist_pct(&h, 99.9), (unsigned long long)h.max);
        tl_hist_free(&h);
    }
    fprintf(f, "\n  ],\n");

    /* the full histogram of the open loop — so that the percentiles can be
     * recomputed and merged with other runs without raw samples */
    fprintf(f, "  \"hist_ns\": [");
    int first = 1;
    for (int i = 0; i < (int)TL_HBUCK; i++) if (lat.b[i]) {
        fprintf(f, "%s[%llu,%u]", first ? "" : ",", (unsigned long long)hval(i), lat.b[i]);
        first = 0;
    }
    fprintf(f, "],\n");
    /* The job's outcome goes INSIDE the report. The receiver used to append `{"error": 1}`
     * AFTER the closing brace, and the file stopped being JSON: the summary silently
     * "skipped" it, and on the EC2 stand all ten points of the Talaria side disappeared that way
     * (the server was not accepting connections — and there was not a word about it,
     * only "skipped ... Extra data"). The reason for the first error is next to it. */
    fprintf(f, "  \"rc\": %d,\n  \"error\": \"", (firsterr || lat.n == 0) ? 1 : 0);
    if (firsterr) json_str(f, firsterr);
    else if (lat.n == 0) fputs("not a single measured answer", f);
    fprintf(f, "\"\n}\n");
    g_job_wrote_json = 1;
    /* We close ONLY what we opened ourselves. `sock_out` is lent by the job
     * receiver (`serve_loop`), and it closes it itself; closing it here made
     * a second `fclose` of the same FILE — a use of freed memory. From the outside
     * it looked like "the generator answers the first job and dies":
     * signal 11 in tlgen right after the first report, identically on Talaria and on
     * Linux. On the EC2 stand that would have broken a run at the very first ladder point. */
    if (f != stdout && f != sock_out) fclose(f);
    else if (f == sock_out) fflush(f);

    if (o.samples_path) {
        FILE *sf = fopen(o.samples_path, "wb");
        if (sf) {
            for (int t = 0; t < o.threads; t++) fwrite(ws[t].smp, sizeof(uint32_t), ws[t].smp_n, sf);
            fclose(sf);
        }
    }


    fprintf(stderr, "tlgen: %.0f req/s, measured %llu, p50=%.1f µs p99=%.1f µs "
                    "p99.9=%.1f µs p99.99=%.1f µs max=%.1f µs%s%s\n",
            thr, (unsigned long long)lat.n,
            tl_hist_pct(&lat, 50) / 1000.0, tl_hist_pct(&lat, 99) / 1000.0,
            tl_hist_pct(&lat, 99.9) / 1000.0, tl_hist_pct(&lat, 99.99) / 1000.0,
            lat.max / 1000.0,
            (overload || (o.rate > 0 && achieved < 98.0))
                ? "  WARNING: overload — the server does not hold the requested rate, the percentiles measure a queue" : "",
            (tmo || cerr) ? "  WARNING: there were timeouts/disconnects" : "");

    /* We free everything: in --serve mode the process lives for the whole stand, and a leak
     * of histograms per job would eat the receiver's memory within a couple of hours. */
    uint64_t measured = lat.n;
    for (int t = 0; t < o.threads; t++) {
        tl_hist_free(&ws[t].lat); tl_hist_free(&ws[t].svc);
        for (int s2 = 0; s2 < nsec; s2++) tl_hist_free(&ws[t].sech[s2]);
        free(ws[t].sech); free(ws[t].secn); free(ws[t].smp);
        for (int i = 0; i < ws[t].nconn; i++) {
            /* The socket is closed by the worker itself — both on a normal exit and on an
             * opening failure. Here is the LAST check: in --serve mode the process lives
             * for the whole stand, and one unclosed socket per job is a failure a dozen
             * points later, and on the SERVER at that (it holds its worker
             * process on it), i.e. far from the place of the leak. */
            if (ws[t].cs[i].fd >= 0) { close(ws[t].cs[i].fd); ws[t].cs[i].fd = -1; }
            if (P->conn_free) P->conn_free(&ws[t].cs[i]);
            free(ws[t].cs[i].rbuf); free(ws[t].cs[i].wbuf);
        }
        free(ws[t].cs);
        if (ws[t].ep >= 0) close(ws[t].ep);
    }
    free(ws); free(th);
    tl_hist_free(&lat); tl_hist_free(&svc);

    return (firsterr || (measured == 0)) ? 1 : 0;
}

/* ─────────────────────── the job receiver mode (--serve) ────────────────────
 *
 * Why. On a "Talaria against Talaria" stand the load generator is the same kind of
 * machine under Talaria, and running twenty different runs on it by rewriting
 * the disk image and rebooting for each one means spending an hour on what
 * takes a minute. So the generator is brought up ONCE and accepts jobs
 * over TCP.
 *
 * The protocol is as simple as it gets: a line of arguments (as on the command line, without
 * the program name) + \n, and in reply a JSON report and a connection close. No
 * authentication: the receiver comes up in the stand's closed contour and lives
 * exactly as long as the stand does.
 */
#define SERVE_MAXARG 128

static int split_args(char *line, char **argv, int maxarg) {
    int n = 0;
    argv[n++] = (char *)"tlgen";
    char *p = line;
    while (*p && n < maxarg - 1) {
        while (*p == ' ' || *p == '\t') p++;
        if (!*p) break;
        char q = 0;
        if (*p == '\'' || *p == '"') { q = *p++; }
        argv[n++] = p;
        if (q) { while (*p && *p != q) p++; }
        else   { while (*p && *p != ' ' && *p != '\t') p++; }
        if (*p) *p++ = 0;
    }
    argv[n] = NULL;
    return n;
}

static int serve_loop(int port) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons((uint16_t)port); a.sin_addr.s_addr = 0;
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0 || listen(s, 8) < 0) {
        fprintf(stderr, "tlgen: could not listen on :%d (%s)\n", port, strerror(errno));
        return 2;
    }
    fprintf(stderr, "tlgen: the job receiver on :%d (a line of arguments -> a JSON report)\n", port);
    /* SIGTERM/SIGINT must stop the receiver too: with `signal()` glibc
     * restarts accept itself (SA_RESTART), and `kill` did not take it at all —
     * the harness's self-check waited for it forever. Without SA_RESTART accept returns
     * EINTR, and the g_stop flag reaches the loop. */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_sig;
    sigaction(SIGINT, &sa, NULL); sigaction(SIGTERM, &sa, NULL);
    for (;;) {
        if (g_stop) { fprintf(stderr, "tlgen: the job receiver was stopped by a signal\n"); break; }
        int c = accept(s, NULL, NULL);
        if (c < 0) {
            if (errno == EINTR) continue;
            /* The receiver lives for the whole run and serves dozens of jobs in a row.
             * A silent exit from the loop looks from the outside like "the generator does not
             * answer from the second point on", and the reason has to be looked for in the guest's
             * kernel; so the failure is printed. */
            fprintf(stderr, "tlgen: accept refused (%s) — the job receiver is closing\n", strerror(errno));
            break;
        }
        FILE *in = fdopen(dup(c), "r");
        FILE *out = fdopen(c, "w");
        if (!in || !out) { close(c); continue; }
        char line[4096];
        if (fgets(line, sizeof line, in)) {
            line[strcspn(line, "\r\n")] = 0;
            /* Print BEFORE parsing: split_args cuts the line in place, putting
             * zeros instead of spaces, and after it only the first argument went
             * into the log ("job: --proto"). */
            fprintf(stderr, "tlgen: job: %s\n", line);
            char *av[SERVE_MAXARG];
            int ac = split_args(line, av, SERVE_MAXARG);
            g_job_wrote_json = 0;
            int rc = run_job(ac, av, out);
            /* There is no report at all (the arguments did not parse) — then, and only
             * then, the answer consists of a single object with a code. */
            if (rc != 0 && !g_job_wrote_json) fprintf(out, "{\"schema\": 1, \"rc\": %d, \"error\": \"the job did not parse\"}\n", rc);
        }
        fflush(out);
        fclose(in); fclose(out);
    }
    close(s);
    return 0;
}

int main(int argc, char **argv) {
    signal(SIGINT, on_sig); signal(SIGTERM, on_sig); signal(SIGPIPE, SIG_IGN);
    for (int i = 1; i < argc; i++)
        if (!strcmp(argv[i], "--serve") && i + 1 < argc) return serve_loop(atoi(argv[i + 1]));
    return run_job(argc, argv, NULL);
}
