// A C++-compatible bridge to the libtsync MPSC (tsync.h uses C11 _Atomic, which
// does not compile as C++; the bodies are in tsync.c, this is a thin C wrapper).
// A queue of u64 values: many producers -> one consumer; on Talaria the consumer
// sleeps with a kind1 upark on the slot's seq word (the producer's store wakes the
// kernel mwait without a syscall), on Linux with a futex. See kernel/user/tsync/tsync.[ch].
#ifndef TSYNC_SHIM_H
#define TSYNC_SHIM_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct tsq tsq; // opaque: tsync_mpsc + the slot buffer

tsq *tsq_new(uint64_t cap_pow2);
void tsq_free(tsq *q);
void tsq_push(tsq *q, uint64_t v);      // blocks when full
uint64_t tsq_pop(tsq *q);               // sleeps when empty
int tsq_try_pop(tsq *q, uint64_t *v);   // -1 = empty
uint64_t tsq_parks(tsq *q);             // diagnostics: consumer parks
int tsq_kmode(void);                    // the Talaria kernel wait mode (-1 = Linux)

#ifdef __cplusplus
}
#endif
#endif
