// libtsync fibers — the implementation. The switch saves what a call preserves: on x86-64
// six registers + the rsp (SysV: the xmm ones are caller-saved, at a call boundary the FPU
// state need not be saved); on arm64 x19-x30 and d8-d15 + the sp (AAPCS64).
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

// switch: save the current one's stack pointer into *from, restore from to.
#if defined(__x86_64__)
// The context on the stack: [rbp][rbx][r12][r13][r14][r15][ret]
__asm__(
    ".globl tfib_switch_asm\n"
    "tfib_switch_asm:\n"        // rdi = &from->rsp, rsi = to->rsp
    "  push %rbp\n  push %rbx\n  push %r12\n  push %r13\n  push %r14\n  push %r15\n"
    "  mov %rsp, (%rdi)\n"
    "  mov %rsi, %rsp\n"
    "  pop %r15\n  pop %r14\n  pop %r13\n  pop %r12\n  pop %rbx\n  pop %rbp\n"
    "  ret\n");
#define TFIB_FRAME_WORDS 6          // under the return address
#elif defined(__aarch64__)
// AAPCS64: x19-x28, the frame pointer x29, the link register x30 and the low halves of
// v8-v15 (d8-d15) survive a call. The context is 160 bytes on the stack:
// [x19 x20][x21 x22][x23 x24][x25 x26][x27 x28][x29 x30][d8 d9][d10 d11][d12 d13][d14 d15]
// and `ret` goes to the restored x30 — for a new fiber, `fiber_entry`.
__asm__(
    ".globl tfib_switch_asm\n"
    "tfib_switch_asm:\n"        // x0 = &from->rsp, x1 = to->rsp
    "  sub sp, sp, #160\n"
    "  stp x19, x20, [sp, #0]\n  stp x21, x22, [sp, #16]\n  stp x23, x24, [sp, #32]\n"
    "  stp x25, x26, [sp, #48]\n  stp x27, x28, [sp, #64]\n  stp x29, x30, [sp, #80]\n"
    "  stp d8, d9, [sp, #96]\n  stp d10, d11, [sp, #112]\n  stp d12, d13, [sp, #128]\n  stp d14, d15, [sp, #144]\n"
    "  mov x2, sp\n  str x2, [x0]\n"
    "  mov sp, x1\n"
    "  ldp x19, x20, [sp, #0]\n  ldp x21, x22, [sp, #16]\n  ldp x23, x24, [sp, #32]\n"
    "  ldp x25, x26, [sp, #48]\n  ldp x27, x28, [sp, #64]\n  ldp x29, x30, [sp, #80]\n"
    "  ldp d8, d9, [sp, #96]\n  ldp d10, d11, [sp, #112]\n  ldp d12, d13, [sp, #128]\n  ldp d14, d15, [sp, #144]\n"
    "  add sp, sp, #160\n"
    "  ret\n");
#endif
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
#if defined(__x86_64__)
    *--sp = 0;                        // the alignment/a stop
    *--sp = (uint64_t)fiber_entry;    // the ret of the switch -> fiber_entry
    for (int i = 0; i < TFIB_FRAME_WORDS; i++) *--sp = 0; // r15..rbp
#elif defined(__aarch64__)
    sp -= 20;                         // the 160-byte context, sp stays 16-aligned
    for (int i = 0; i < 20; i++) sp[i] = 0;
    sp[11] = (uint64_t)fiber_entry;   // x30: the switch's `ret` -> fiber_entry (x29 = 0 ends the frame chain)
#endif
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
