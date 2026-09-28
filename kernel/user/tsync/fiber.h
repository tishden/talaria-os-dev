// libtsync fibers (PLAN-SYNC stage 4): green threads on one core — a switch at the level
// of a coroutine (the callee-saved ones + the rsp, ~20-30 ns, without XSAVE/CR3/TLB)
// under the standard semantics of a mutex/condvar: cv.wait = a switch to the next
// runnable one, notify = a move into runnable (+ notify_switch — an immediate handoff).
#ifndef TSYNC_FIBER_H
#define TSYNC_FIBER_H
#include <stddef.h>
#include <stdint.h>

#define TFIB_MAX 64

typedef struct tsync_fiber {
    void *rsp;                  // the saved stack (the context = 6 callee-saved + the ret)
    struct tsync_fiber *next;   // the queue of the runnable / the waiting ones
    int state;                  // 0 free, 1 runnable, 2 running, 3 blocked, 4 dead
    void (*fn)(void *);
    void *arg;
} tsync_fiber;

// Initialise the scheduler of the fibers in the CURRENT thread (it becomes the fiber 0).
void tfib_init(void);
// Create a fiber on the given stack (sz bytes). It returns NULL on an overflow.
tsync_fiber *tfib_spawn(void (*fn)(void *), void *arg, void *stack, size_t sz);
// Give control to the next runnable one (cooperatively).
void tfib_yield(void);
// The current fiber.
tsync_fiber *tfib_self(void);
// Wait for the fiber f to finish (by switches, without blocking the thread).
void tfib_join(tsync_fiber *f);

// A fiber-aware condvar: a sleep = a switch (without the kernel and without a spin at all)
typedef struct { tsync_fiber *head, *tail; } tfib_cond;
#define TFIB_COND_INIT {0, 0}
void tfib_cond_wait(tfib_cond *c);            // put itself into the queue, switch
void tfib_cond_signal(tfib_cond *c);          // the first waiter -> runnable
void tfib_cond_signal_switch(tfib_cond *c);   // + an immediate handoff to it
// A fiber-aware mutex (between the fibers of one thread)
typedef struct { tsync_fiber *owner; tfib_cond waiters; } tfib_mutex;
#define TFIB_MUTEX_INIT {0, TFIB_COND_INIT}
void tfib_mutex_lock(tfib_mutex *m);
void tfib_mutex_unlock(tfib_mutex *m);

#endif
