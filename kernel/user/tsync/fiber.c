// libtsync fibers — the implementation. The switch: 6 callee-saved + the rsp (SysV: the
// xmm ones are caller-saved, at a call boundary the FPU state need not be saved).
#include "fiber.h"

static tsync_fiber fibers[TFIB_MAX];
static tsync_fiber *cur;
static tsync_fiber *rq_head, *rq_tail; // runnable (a circular FIFO)

static void rq_push(tsync_fiber *f) {
    f->next = 0; f->state = 1;
    if (rq_tail) rq_tail->next = f; else rq_head = f;
    rq_tail = f;
}
static tsync_fiber *rq_pop(void) {
    tsync_fiber *f = rq_head;
    if (f) { rq_head = f->next; if (!rq_head) rq_tail = 0; f->next = 0; }
    return f;
}

// switch: save the current one's rsp into *from, restore from to. The context on the stack:
// [rbp][rbx][r12][r13][r14][r15][ret]
__asm__(
    ".globl tfib_switch_asm\n"
    "tfib_switch_asm:\n"        // rdi = &from->rsp, rsi = to->rsp
    "  push %rbp\n  push %rbx\n  push %r12\n  push %r13\n  push %r14\n  push %r15\n"
    "  mov %rsp, (%rdi)\n"
    "  mov %rsi, %rsp\n"
    "  pop %r15\n  pop %r14\n  pop %r13\n  pop %r12\n  pop %rbx\n  pop %rbp\n"
    "  ret\n");
extern void tfib_switch_asm(void **from_rsp, void *to_rsp);

static void switch_to(tsync_fiber *to) {
    tsync_fiber *from = cur;
    cur = to; to->state = 2;
    tfib_switch_asm(&from->rsp, to->rsp);
}

void tfib_init(void) {
    for (int i = 0; i < TFIB_MAX; i++) fibers[i].state = 0;
    cur = &fibers[0];
    cur->state = 2; // the current thread = the fiber 0 (the context will be filled at the first switch)
}
tsync_fiber *tfib_self(void) { return cur; }

static void fiber_entry(void);

tsync_fiber *tfib_spawn(void (*fn)(void *), void *arg, void *stack, size_t sz) {
    tsync_fiber *f = 0;
    for (int i = 1; i < TFIB_MAX; i++) if (fibers[i].state == 0 || fibers[i].state == 4) { f = &fibers[i]; break; }
    if (!f) return 0;
    f->fn = fn; f->arg = arg;
    uint64_t *sp = (uint64_t *)(((uintptr_t)stack + sz) & ~0xFULL);
    *--sp = 0;                        // the alignment/a stop
    *--sp = (uint64_t)fiber_entry;    // the ret of the switch -> fiber_entry
    *--sp = 0; *--sp = 0; *--sp = 0; *--sp = 0; *--sp = 0; *--sp = 0; // r15..rbp
    f->rsp = sp;
    rq_push(f);
    return f;
}

static void fiber_entry(void) {
    tsync_fiber *me = cur;
    me->fn(me->arg);
    me->state = 4; // dead
    tsync_fiber *n = rq_pop();
    while (!n) n = rq_pop(); // there is nobody to pass to — the program is incorrect; we spin
    switch_to(n);            // it will not return
    __builtin_unreachable();
}

void tfib_yield(void) {
    tsync_fiber *n = rq_pop();
    if (!n) return;         // there is nobody else — we go on ourselves
    rq_push(cur);
    switch_to(n);
}

void tfib_join(tsync_fiber *f) {
    while (f->state != 4) tfib_yield();
}

// ── fiber condvar/mutex ──────────────────────────────────────────────────────
void tfib_cond_wait(tfib_cond *c) {
    tsync_fiber *me = cur;
    me->state = 3; me->next = 0;
    if (c->tail) c->tail->next = me; else c->head = me;
    c->tail = me;
    tsync_fiber *n = rq_pop();
    while (!n) n = rq_pop(); // all are blocked = a deadlock of the application
    switch_to(n);
}
static tsync_fiber *cond_pop(tfib_cond *c) {
    tsync_fiber *f = c->head;
    if (f) { c->head = f->next; if (!c->head) c->tail = 0; f->next = 0; }
    return f;
}
void tfib_cond_signal(tfib_cond *c) {
    tsync_fiber *f = cond_pop(c);
    if (f) rq_push(f);
}
void tfib_cond_signal_switch(tfib_cond *c) {
    tsync_fiber *f = cond_pop(c);
    if (!f) return;
    f->state = 1;
    rq_push(cur);      // we stay runnable
    switch_to(f);      // an immediate handoff (the latency = one switch)
}

void tfib_mutex_lock(tfib_mutex *m) {
    while (m->owner) tfib_cond_wait(&m->waiters); // a single-threaded vcore: a spin is meaningless
    m->owner = cur;
}
void tfib_mutex_unlock(tfib_mutex *m) {
    m->owner = 0;
    tfib_cond_signal(&m->waiters);
}
