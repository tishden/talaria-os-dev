// A C11 -> C++ bridge for the libtsync MPSC (see tsync_shim.h).
#include "tsync.h"
#include "tsync_shim.h"
#include <stdlib.h>

struct tsq {
    tsync_mpsc q;
    tsync_mpsc_slot *buf;
};

tsq *tsq_new(uint64_t cap_pow2) {
    tsq *w = aligned_alloc(64, sizeof(tsq));
    if (!w) return 0;
    w->buf = aligned_alloc(64, cap_pow2 * sizeof(tsync_mpsc_slot));
    if (!w->buf) { free(w); return 0; }
    if (tsync_mpsc_init(&w->q, cap_pow2, w->buf) != 0) { free(w->buf); free(w); return 0; }
    return w;
}
void tsq_free(tsq *w) { if (w) { free(w->buf); free(w); } }
void tsq_push(tsq *w, uint64_t v) { tsync_mpsc_push(&w->q, v); }
uint64_t tsq_pop(tsq *w) { return tsync_mpsc_pop(&w->q); }
int tsq_try_pop(tsq *w, uint64_t *v) { return tsync_mpsc_try_pop(&w->q, v); }
uint64_t tsq_parks(tsq *w) { return w->q.c_parks; }
int tsq_kmode(void) { return tsync_kmode(); }
