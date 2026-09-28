// actor-bench — a benchmark of the actor model (SObjectizer-5) for Talaria vs Linux.
// One and the same static musl binary runs on both stands (the same
// QEMU/KVM VM, -smp 4): what is compared is the wakeup path of actor threads —
// futex/condvar (Linux) against a kernel mwait on the user word (Talaria, PLAN-SYNC).
//
// Scenarios (kinds of load):
//   pingpong — request/response RTT between two actors (1 in flight): pure
//              latency of message delivery + waking the actor's thread;
//   ring     — a ring of R actors, one token: the cost of a relay (hop);
//   fanout   — scatter-gather 1->W->1 (collective wakeup and gathering);
//   pipeline — a 4-stage pipeline, W messages in flight: end-to-end latency
//              under a stream + throughput;
//   flood    — a 1->1 stream with a credit window: pure throughput of an actor queue.
//
// Dispatchers (distribution over threads and cores):
//   so       — so_5::disp::one_thread, the stock queue, combined_lock
//              (busy-spin up to 1 ms before sleeping) — "out of the box";
//   so-simple— the same with simple_lock (mutex + condvar without spinning) — the
//              honest futex path: on Talaria it sleeps in a kernel mwait;
//   tsync    — a custom dispatcher: one thread per actor, a tsync_mpsc queue
//              (Vyukov MPSC; sleeping = a kind1 upark on the slot's seq word, the
//              producer's store wakes it without a syscall) — "the refined concurrency API";
//   raw      — the same topology without the framework (threads + tsync_mpsc directly):
//              the lower bound, shows the framework's overhead.
//
// Pinning: none — no thread is pinned (on Talaria = all on the BSP);
//          spread — actor i -> core i (on Talaria at most ONE thread per AP:
//          APs run to completion; extra actors stay on core 0).
//
// Usage: actor-bench <scen> <disp> <pin> [N] [REPEATS]  |  actor-bench all [REPEATS]
// Output: lines "RESULT scen=... disp=... pin=... n=... secs=... p50=..."
#define _GNU_SOURCE 1
#include <so_5/all.hpp>
#include "tsync_shim.h"

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <cstdlib>
#include <string>
#include <thread>
#include <vector>

#include <sched.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

// ── time and statistics helpers ─────────────────────────────────────────────
static inline uint64_t rdtsc() {
    unsigned lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (uint64_t)hi << 32 | lo;
}
static uint64_t g_hz;
static uint64_t now_ns_clock() {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec;
}
static void calibrate_tsc() {
    uint64_t n0 = now_ns_clock(), c0 = rdtsc();
    struct timespec d = {0, 200 * 1000 * 1000}; nanosleep(&d, 0);
    uint64_t n1 = now_ns_clock(), c1 = rdtsc();
    g_hz = (c1 - c0) * 1000000000ull / (n1 - n0);
}
static inline uint64_t cyc2ns(uint64_t c) { return c * 1000ull / (g_hz / 1000000ull); }

static void pin_to(int cpu) {
    if (cpu < 0) return;
    cpu_set_t s; CPU_ZERO(&s); CPU_SET(cpu, &s);
    syscall(SYS_sched_setaffinity, 0, sizeof(s), &s);
}
static int ncpu() {
    unsigned long m = 0;
    if (syscall(SYS_sched_getaffinity, 0, sizeof(m), &m) < 0 || !m) return 1;
    return __builtin_popcountl(m);
}
// Placement: actor idx out of A actors. Talaria: at most 1 thread per AP —
// spread gives cores 1..ncpu-1 once each, the rest go to 0.
static int pin_cpu(const std::string &pin, int idx) {
    if (pin == "none") return -1;
    if (pin == "same") return 0;
    int n = ncpu();
    return (idx < n) ? idx : 0;
}

struct stats_t {
    std::vector<uint32_t> lat;    // cycles
    uint64_t n = 0;               // events (for thr)
    uint64_t cyc = 0;             // duration of the scenario in cycles
};
static stats_t g_st;

// THE REPEAT NUMBER IN THE LINE ITSELF. The matrix varies between runs more than
// the difference we look for, and it could not be told WHERE the noise lives: in
// the scenario or in the VM boot itself. A repeat within one boot separates the
// two — and that decides what buys precision, an extra boot or an extra repeat.
static int g_rep = 0;

static void report(const char *scen, const std::string &disp, const std::string &pin) {
    auto &v = g_st.lat;
    double secs = (double)g_st.cyc / (double)g_hz;
    uint64_t p50 = 0, p90 = 0, p99 = 0, p999 = 0, mx = 0;
    size_t n = v.size();
    if (!n && g_st.n) { // the deadline ate everything before warm-up ended: at least the mean instead of zeros
        p50 = p90 = p99 = p999 = mx = cyc2ns(g_st.cyc / g_st.n);
    }
    if (n) {
        std::sort(v.begin(), v.end());
        p50 = cyc2ns(v[n / 2]); p90 = cyc2ns(v[n * 9 / 10]);
        p99 = cyc2ns(v[n * 99 / 100]); p999 = cyc2ns(v[n * 999 / 1000]);
        mx = cyc2ns(v[n - 1]);
    }
    double thr = secs > 0 ? (double)g_st.n / secs : 0;
    std::printf("RESULT scen=%s disp=%s pin=%s rep=%d n=%llu secs=%.3f p50=%llu p90=%llu p99=%llu p999=%llu max=%llu thr=%.0f\n",
        scen, disp.c_str(), pin.c_str(), g_rep, (unsigned long long)g_st.n, secs,
        (unsigned long long)p50, (unsigned long long)p90, (unsigned long long)p99,
        (unsigned long long)p999, (unsigned long long)mx, thr);
    std::fflush(stdout);
    g_st = stats_t{};
}

static constexpr uint64_t DEADLINE_S = 10; // scenario ceiling (spinning configs on 1 core are slow)

// ── a pinning thread factory for the stock so_5 dispatchers ─────────────────
class pin_thread_t final : public so_5::disp::abstract_work_thread_t {
    std::thread m_t; int m_cpu;
public:
    explicit pin_thread_t(int cpu) : m_cpu(cpu) {}
    void start(body_func_t body) override {
        m_t = std::thread([cpu = m_cpu, b = std::move(body)] { pin_to(cpu); b(); });
    }
    void join() override { m_t.join(); }
};
class pin_factory_t final : public so_5::disp::abstract_work_thread_factory_t {
    int m_cpu;
public:
    explicit pin_factory_t(int cpu) : m_cpu(cpu) {}
    so_5::disp::abstract_work_thread_t &acquire(so_5::environment_t &) override {
        return *new pin_thread_t(m_cpu);
    }
    void release(so_5::disp::abstract_work_thread_t &t) noexcept override { delete &t; }
};

// The stock one_thread dispatcher: lock_factory by variant + pinning of the thread.
static so_5::disp_binder_shptr_t make_so_binder(
    so_5::environment_t &env, const std::string &disp, int cpu) {
    namespace ot = so_5::disp::one_thread;
    ot::disp_params_t p;
    p.work_thread_factory(std::make_shared<pin_factory_t>(cpu));
    if (disp == "so-simple")
        p.tune_queue_params([](ot::queue_traits::queue_params_t &qp) {
            qp.lock_factory(ot::queue_traits::simple_lock_factory());
        });
    return ot::make_dispatcher(env, std::string_view{}, std::move(p)).binder();
}

// ── the custom tsync dispatcher: a thread per actor + a tsync_mpsc queue ────
class tsync_disp_t final : public so_5::event_queue_t {
    struct node_t { so_5::execution_demand_t d; };
    tsq *m_q;
    std::thread m_thr;
public:
    explicit tsync_disp_t(int cpu) : m_q(tsq_new(16384)) {
        m_thr = std::thread([this, cpu] {
            pin_to(cpu);
            auto tid = so_5::query_current_thread_id();
            for (;;) {
                uint64_t v = tsq_pop(m_q);
                if (!v) break; // stop sentinel
                node_t *n = reinterpret_cast<node_t *>(v);
                n->d.call_handler(tid);
                delete n;
            }
        });
    }
    ~tsync_disp_t() override {
        tsq_push(m_q, 0);
        m_thr.join();
        tsq_free(m_q);
    }
    void push(so_5::execution_demand_t d) override {
        tsq_push(m_q, reinterpret_cast<uint64_t>(new node_t{std::move(d)}));
    }
    void push_evt_start(so_5::execution_demand_t d) override { push(std::move(d)); }
    void push_evt_finish(so_5::execution_demand_t d) noexcept override {
        try { push(std::move(d)); } catch (...) { std::terminate(); }
    }
};
class tsync_binder_t final : public so_5::disp_binder_t {
    std::shared_ptr<tsync_disp_t> m_disp;
public:
    explicit tsync_binder_t(std::shared_ptr<tsync_disp_t> d) : m_disp(std::move(d)) {}
    void preallocate_resources(so_5::agent_t &) override {}
    void undo_preallocation(so_5::agent_t &) noexcept override {}
    void bind(so_5::agent_t &a) noexcept override { a.so_bind_to_dispatcher(*m_disp); }
    void unbind(so_5::agent_t &) noexcept override {}
};

// Owner of the scenario's tsync dispatchers: destroyed AFTER env (the queue
// threads must live until every agent has handled evt_finish).
struct disp_pool_t {
    std::vector<std::shared_ptr<tsync_disp_t>> tsync;
    so_5::disp_binder_shptr_t binder(so_5::environment_t &env,
        const std::string &disp, int cpu) {
        if (disp == "tsync") {
            auto d = std::make_shared<tsync_disp_t>(cpu);
            tsync.push_back(d);
            return std::make_shared<tsync_binder_t>(d);
        }
        return make_so_binder(env, disp, cpu);
    }
};

// ── signals and messages ────────────────────────────────────────────────────
struct sig_ping final : so_5::signal_t {};
struct sig_pong final : so_5::signal_t {};
struct sig_work final : so_5::signal_t {};
struct sig_done final : so_5::signal_t {};
struct sig_item final : so_5::signal_t {};
struct sig_ack  final : so_5::signal_t {};
struct sig_stop final : so_5::signal_t {};
struct msg_tok  final : so_5::message_t { int left; explicit msg_tok(int l) : left(l) {} };
struct msg_pipe final : so_5::message_t { uint64_t t0; explicit msg_pipe(uint64_t t) : t0(t) {} };

// ── pingpong ─────────────────────────────────────────────────────────────────
class ponger_t final : public so_5::agent_t {
    so_5::mbox_t m_peer;
public:
    using so_5::agent_t::agent_t;
    void set_peer(so_5::mbox_t m) { m_peer = std::move(m); }
    void so_define_agent() override {
        so_subscribe_self().event([this](so_5::mhood_t<sig_ping>) {
            so_5::send<sig_pong>(m_peer);
        });
    }
};
class pinger_t final : public so_5::agent_t {
    so_5::mbox_t m_peer;
    uint64_t m_n, m_warm, m_i = 0, m_t0 = 0, m_start = 0, m_deadline;
public:
    pinger_t(context_t ctx, uint64_t n)
        : so_5::agent_t(std::move(ctx)), m_n(n), m_warm(n / 20 + 1),
          m_deadline(rdtsc() + DEADLINE_S * g_hz) { g_st.lat.reserve(n); }
    void set_peer(so_5::mbox_t m) { m_peer = std::move(m); }
    void so_evt_start() override {
        m_start = rdtsc(); m_t0 = m_start;
        so_5::send<sig_ping>(m_peer);
    }
    void so_define_agent() override {
        so_subscribe_self().event([this](so_5::mhood_t<sig_pong>) {
            uint64_t now = rdtsc();
            if (m_i >= m_warm) g_st.lat.push_back((uint32_t)std::min<uint64_t>(now - m_t0, ~0u));
            if (++m_i >= m_n || ((m_i & 255) == 0 && now > m_deadline)) {
                g_st.n = m_i; g_st.cyc = now - m_start;
                so_environment().stop();
                return;
            }
            m_t0 = rdtsc();
            so_5::send<sig_ping>(m_peer);
        });
    }
};

// ── ring ─────────────────────────────────────────────────────────────────────
class ring_node_t final : public so_5::agent_t {
    so_5::mbox_t m_next;
public:
    using so_5::agent_t::agent_t;
    void set_next(so_5::mbox_t m) { m_next = std::move(m); }
    void so_define_agent() override {
        so_subscribe_self().event([this](so_5::mhood_t<msg_tok> m) {
            if (m->left > 0) so_5::send<msg_tok>(m_next, m->left - 1);
        });
    }
};
class ring_head_t final : public so_5::agent_t {
    so_5::mbox_t m_next;
    int m_ring;
    uint64_t m_laps, m_warm, m_i = 0, m_t0 = 0, m_start = 0, m_deadline;
public:
    ring_head_t(context_t ctx, int ring, uint64_t laps)
        : so_5::agent_t(std::move(ctx)), m_ring(ring), m_laps(laps),
          m_warm(laps / 20 + 1), m_deadline(rdtsc() + DEADLINE_S * g_hz) { g_st.lat.reserve(laps); }
    void set_next(so_5::mbox_t m) { m_next = std::move(m); }
    void so_evt_start() override {
        m_start = rdtsc(); m_t0 = m_start;
        so_5::send<msg_tok>(m_next, m_ring - 1); // r-1 intermediate hops, it comes back to us
    }
    void so_define_agent() override {
        so_subscribe_self().event([this](so_5::mhood_t<msg_tok>) {
            uint64_t now = rdtsc();
            // latency of ONE hop = lap time / number of hops (ring)
            if (m_i >= m_warm) g_st.lat.push_back((uint32_t)std::min<uint64_t>((now - m_t0) / m_ring, ~0u));
            if (++m_i >= m_laps || ((m_i & 63) == 0 && now > m_deadline)) {
                g_st.n = m_i * m_ring; g_st.cyc = now - m_start;
                so_environment().stop();
                return;
            }
            m_t0 = rdtsc();
            so_5::send<msg_tok>(m_next, m_ring - 1);
        });
    }
};

// ── fanout (scatter-gather) ──────────────────────────────────────────────────
class worker_t final : public so_5::agent_t {
    so_5::mbox_t m_coord;
public:
    using so_5::agent_t::agent_t;
    void set_coord(so_5::mbox_t m) { m_coord = std::move(m); }
    void so_define_agent() override {
        so_subscribe_self().event([this](so_5::mhood_t<sig_work>) {
            so_5::send<sig_done>(m_coord);
        });
    }
};
class coordinator_t final : public so_5::agent_t {
    std::vector<so_5::mbox_t> m_workers;
    uint64_t m_rounds, m_warm, m_i = 0, m_got = 0, m_t0 = 0, m_start = 0, m_deadline;
public:
    coordinator_t(context_t ctx, uint64_t rounds)
        : so_5::agent_t(std::move(ctx)), m_rounds(rounds), m_warm(rounds / 20 + 1),
          m_deadline(rdtsc() + DEADLINE_S * g_hz) { g_st.lat.reserve(rounds); }
    void add_worker(so_5::mbox_t m) { m_workers.push_back(std::move(m)); }
    void scatter() { for (auto &w : m_workers) so_5::send<sig_work>(w); }
    void so_evt_start() override { m_start = rdtsc(); m_t0 = m_start; scatter(); }
    void so_define_agent() override {
        so_subscribe_self().event([this](so_5::mhood_t<sig_done>) {
            if (++m_got < m_workers.size()) return;
            m_got = 0;
            uint64_t now = rdtsc();
            if (m_i >= m_warm) g_st.lat.push_back((uint32_t)std::min<uint64_t>(now - m_t0, ~0u));
            if (++m_i >= m_rounds || ((m_i & 63) == 0 && now > m_deadline)) {
                g_st.n = m_i * m_workers.size(); g_st.cyc = now - m_start;
                so_environment().stop();
                return;
            }
            m_t0 = rdtsc();
            scatter();
        });
    }
};

// ── pipeline (4 stages, W in flight) ────────────────────────────────────────
class stage_t final : public so_5::agent_t {
    so_5::mbox_t m_next;
public:
    using so_5::agent_t::agent_t;
    void set_next(so_5::mbox_t m) { m_next = std::move(m); }
    void so_define_agent() override {
        so_subscribe_self().event([this](so_5::mhood_t<msg_pipe> m) {
            so_5::send<msg_pipe>(m_next, m->t0);
        });
    }
};
class injector_t final : public so_5::agent_t {
    so_5::mbox_t m_first;
    uint64_t m_total, m_window, m_sent = 0, m_done = 0, m_start = 0, m_deadline;
    bool m_stopping = false;
public:
    injector_t(context_t ctx, uint64_t total, uint64_t window)
        : so_5::agent_t(std::move(ctx)), m_total(total), m_window(window),
          m_deadline(rdtsc() + DEADLINE_S * g_hz) { g_st.lat.reserve(total); }
    void set_first(so_5::mbox_t m) { m_first = std::move(m); }
    void so_evt_start() override {
        m_start = rdtsc();
        for (uint64_t i = 0; i < m_window && m_sent < m_total; i++, m_sent++)
            so_5::send<msg_pipe>(m_first, rdtsc());
    }
    void so_define_agent() override {
        so_subscribe_self().event([this](so_5::mhood_t<msg_pipe> m) {
            uint64_t now = rdtsc();
            g_st.lat.push_back((uint32_t)std::min<uint64_t>(now - m->t0, ~0u));
            m_done++;
            if (((m_done & 255) == 0 && now > m_deadline)) m_stopping = true;
            if (!m_stopping && m_sent < m_total) {
                m_sent++;
                so_5::send<msg_pipe>(m_first, rdtsc());
            } else if (m_done == m_sent) {
                g_st.n = m_done; g_st.cyc = rdtsc() - m_start;
                so_environment().stop();
            }
        });
    }
};

// ── flood (throughput, credit window) ───────────────────────────────────────
static constexpr uint64_t FLOOD_CHUNK = 4096;
class sink_t final : public so_5::agent_t {
    so_5::mbox_t m_prod;
    uint64_t m_got = 0;
public:
    using so_5::agent_t::agent_t;
    void set_prod(so_5::mbox_t m) { m_prod = std::move(m); }
    void so_define_agent() override {
        so_subscribe_self()
            .event([this](so_5::mhood_t<sig_item>) {
                if ((++m_got % FLOOD_CHUNK) == 0) so_5::send<sig_ack>(m_prod);
            })
            .event([this](so_5::mhood_t<sig_stop>) {
                so_5::send<sig_done>(m_prod);
            });
    }
};
class flooder_t final : public so_5::agent_t {
    so_5::mbox_t m_sink;
    uint64_t m_total, m_sent = 0, m_start = 0, m_deadline;
    bool m_stopped = false;
public:
    flooder_t(context_t ctx, uint64_t total)
        : so_5::agent_t(std::move(ctx)), m_total(total),
          m_deadline(rdtsc() + DEADLINE_S * g_hz) {}
    void set_sink(so_5::mbox_t m) { m_sink = std::move(m); }
    // A chunk + an immediate stop after the LAST one (an incomplete chunk gets no
    // ack: the sink acks every FLOOD_CHUNK received — stop cannot hang on an ack).
    void chunk() {
        uint64_t n = std::min(FLOOD_CHUNK, m_total - m_sent);
        for (uint64_t i = 0; i < n; i++) so_5::send<sig_item>(m_sink);
        m_sent += n;
        if (m_sent >= m_total || rdtsc() > m_deadline) stop_once();
    }
    void stop_once() {
        if (!m_stopped) { m_stopped = true; so_5::send<sig_stop>(m_sink); }
    }
    // Two chunks in flight (double buffering): a one-chunk window serialises the
    // producer's burst and the consumer's drain (throughput fell to 1/(X+Y)).
    void so_evt_start() override { m_start = rdtsc(); chunk(); if (!m_stopped) chunk(); }
    void so_define_agent() override {
        so_subscribe_self()
            .event([this](so_5::mhood_t<sig_ack>) {
                if (m_stopped) return;
                if (m_sent < m_total && rdtsc() < m_deadline) chunk();
                else stop_once();
            })
            .event([this](so_5::mhood_t<sig_done>) {
                g_st.n = m_sent; g_st.cyc = rdtsc() - m_start;
                so_environment().stop();
            });
    }
};

// ── raw mode (no framework): the lower bound on tsync_mpsc ──────────────────
static void raw_pingpong(const std::string &pin, uint64_t n) {
    tsq *qa = tsq_new(4096), *qb = tsq_new(4096);
    std::thread b([&] {
        pin_to(pin_cpu(pin, 1));
        for (;;) {
            uint64_t v = tsq_pop(qb);
            tsq_push(qa, v);
            if (v == 2) break;
        }
    });
    pin_to(pin_cpu(pin, 0));
    uint64_t warm = n / 20 + 1, deadline = rdtsc() + DEADLINE_S * g_hz;
    g_st.lat.reserve(n);
    uint64_t i = 0, start = rdtsc();
    for (; i < n; i++) {
        uint64_t t0 = rdtsc();
        tsq_push(qb, 1);
        tsq_pop(qa);
        if (i >= warm) g_st.lat.push_back((uint32_t)std::min<uint64_t>(rdtsc() - t0, ~0u));
        if ((i & 255) == 0 && rdtsc() > deadline) { i++; break; }
    }
    g_st.n = i; g_st.cyc = rdtsc() - start;
    tsq_push(qb, 2); // the final lap + stop the partner
    tsq_pop(qa);
    b.join();
    tsq_free(qa); tsq_free(qb);
    pin_to(0); // nowhere to return the main thread to (Talaria BSP), a no-op on Linux
}
static void raw_flood(const std::string &pin, uint64_t n) {
    tsq *q = tsq_new(4096);
    std::thread c([&] {
        pin_to(pin_cpu(pin, 1));
        while (tsq_pop(q) != 2) {}
    });
    pin_to(pin_cpu(pin, 0));
    uint64_t start = rdtsc(), deadline = start + DEADLINE_S * g_hz;
    uint64_t i = 0;
    for (; i < n; i++) {
        tsq_push(q, 1);
        if ((i & 4095) == 0 && rdtsc() > deadline) { i++; break; }
    }
    tsq_push(q, 2);
    c.join();
    g_st.n = i; g_st.cyc = rdtsc() - start;
    tsq_free(q);
}

// ── assembling the so_5 scenarios ───────────────────────────────────────────
static void run_so_scenario(const std::string &scen, const std::string &disp,
    const std::string &pin, uint64_t n) {
    disp_pool_t pool;
    so_5::launch([&](so_5::environment_t &env) {
        env.introduce_coop([&](so_5::coop_t &coop) {
            if (scen == "pingpong") {
                auto *a = coop.make_agent_with_binder<pinger_t>(pool.binder(env, disp, pin_cpu(pin, 0)), n);
                auto *b = coop.make_agent_with_binder<ponger_t>(pool.binder(env, disp, pin_cpu(pin, 1)));
                a->set_peer(b->so_direct_mbox());
                b->set_peer(a->so_direct_mbox());
            } else if (scen == "ring") {
                const int R = 8;
                auto *head = coop.make_agent_with_binder<ring_head_t>(pool.binder(env, disp, pin_cpu(pin, 0)), R, n);
                std::vector<ring_node_t *> nodes;
                for (int i = 1; i < R; i++)
                    nodes.push_back(coop.make_agent_with_binder<ring_node_t>(pool.binder(env, disp, pin_cpu(pin, i))));
                for (size_t i = 0; i + 1 < nodes.size(); i++) nodes[i]->set_next(nodes[i + 1]->so_direct_mbox());
                nodes.back()->set_next(head->so_direct_mbox());
                head->set_next(nodes.front()->so_direct_mbox());
            } else if (scen == "fanout") {
                const int W = 3;
                auto *c = coop.make_agent_with_binder<coordinator_t>(pool.binder(env, disp, pin_cpu(pin, 0)), n);
                for (int i = 0; i < W; i++) {
                    auto *w = coop.make_agent_with_binder<worker_t>(pool.binder(env, disp, pin_cpu(pin, i + 1)));
                    w->set_coord(c->so_direct_mbox());
                    c->add_worker(w->so_direct_mbox());
                }
            } else if (scen == "pipeline") {
                const int S = 3; // + the injector-sink = 4 threads
                auto *inj = coop.make_agent_with_binder<injector_t>(pool.binder(env, disp, pin_cpu(pin, 0)), n, 256);
                std::vector<stage_t *> st;
                for (int i = 0; i < S; i++)
                    st.push_back(coop.make_agent_with_binder<stage_t>(pool.binder(env, disp, pin_cpu(pin, i + 1))));
                inj->set_first(st[0]->so_direct_mbox());
                for (int i = 0; i + 1 < S; i++) st[i]->set_next(st[i + 1]->so_direct_mbox());
                st[S - 1]->set_next(inj->so_direct_mbox());
            } else if (scen == "flood") {
                auto *f = coop.make_agent_with_binder<flooder_t>(pool.binder(env, disp, pin_cpu(pin, 0)), n);
                auto *s = coop.make_agent_with_binder<sink_t>(pool.binder(env, disp, pin_cpu(pin, 1)));
                f->set_sink(s->so_direct_mbox());
                s->set_prod(f->so_direct_mbox());
            }
        });
    });
}

static uint64_t default_n(const std::string &scen) {
    if (scen == "pingpong") return 100000;
    if (scen == "ring")     return 20000;   // laps x8 hops
    if (scen == "fanout")   return 50000;
    if (scen == "pipeline") return 300000;
    if (scen == "flood")    return 3000000;
    return 100000;
}

static void run_one(const std::string &scen, const std::string &disp,
    const std::string &pin, uint64_t n) {
    if (!n) n = default_n(scen);
    if (disp == "raw") {
        if (scen == "pingpong") raw_pingpong(pin, n);
        else if (scen == "flood") raw_flood(pin, n);
        else { std::printf("SKIP scen=%s disp=raw (not implemented)\n", scen.c_str()); return; }
    } else {
        run_so_scenario(scen, disp, pin, n);
    }
    report(scen.c_str(), disp, pin);
}

int main(int argc, char **argv) {
    calibrate_tsc();
    std::printf("MACH ncpu=%d kmode=%d hz=%llu\n", ncpu(), tsq_kmode(), (unsigned long long)g_hz);
    std::fflush(stdout);
    std::string scen = argc > 1 ? argv[1] : "all";
    if (scen != "all") {
        std::string disp = argc > 2 ? argv[2] : "so-simple";
        std::string pin = argc > 3 ? argv[3] : "spread";
        uint64_t n = argc > 4 ? strtoull(argv[4], 0, 10) : 0;
        int reps = argc > 5 ? atoi(argv[5]) : 1;
        for (g_rep = 1; g_rep <= (reps > 0 ? reps : 1); g_rep++) run_one(scen, disp, pin, n);
        std::printf("BENCH DONE\n");
        return 0;
    }
    int reps = argc > 2 ? atoi(argv[2]) : 1;
    if (reps <= 0) reps = 1;
    // the canonical matrix (fixed in the repo; runs on both stands)
    const char *pins[] = {"none", "spread"};
    for (g_rep = 1; g_rep <= reps; g_rep++)
    for (auto pin : pins) {
        for (auto disp : {"so", "so-simple", "tsync", "raw"})
            run_one("pingpong", disp, pin, 0);
        for (auto disp : {"so", "so-simple", "tsync"}) {
            run_one("ring", disp, pin, 0);
            run_one("fanout", disp, pin, 0);
            run_one("pipeline", disp, pin, 0);
        }
        for (auto disp : {"so", "so-simple", "tsync", "raw"})
            run_one("flood", disp, pin, 0);
    }
    std::printf("BENCH DONE\n");
    return 0;
}
