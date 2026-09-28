// mdpipe — a teaching multithreaded workload for Talaria OS.
//
// A pipeline built like a real datapath: one thread ingests a stream of
// quotes, a pool of threads processes them, and a separate thread writes the
// result to disk. What is measured is what Talaria exists for — THE STAGE
// LATENCY, i.e. the time from queueing a tick to the end of its processing, by percentiles.
//
// The threads are named (prctl(PR_SET_NAME)) — those names show up in `top -t`,
// and the manifest places the threads on cores by them (`pin=md_work*:...`).
//
// Arguments:
//   --secs N        how many seconds to drive the stream (default 10)
//   --workers K     worker threads (default 2)
//   --rate R        ticks per second (default 20000)
//   --out PATH      the result file (default /mdpipe.log)
//   --crash-at N    at tick N, dereference an instrument that does not exist (for the
//                   post-mortem chapter: SIGSEGV -> core dump -> gdb bt)
#include <algorithm>
#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cerrno>
#include <ctime>
#include <deque>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <fcntl.h>
#include <sys/prctl.h>
#include <unistd.h>

static inline uint64_t now_ns() {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

// ---- the market model ------------------------------------------------------

struct Tick {
    uint64_t seq;
    uint64_t t_enq;   // when the tick was put into the queue
    uint32_t sym;     // the instrument index
    uint32_t px;      // the price in ticks
};

// The instrument table. The hole is deliberate: sym == BAD_SYM is not set up,
// and the pointer to it is null — the post-mortem chapter is built on that.
struct Instrument {
    char     name[8];
    uint64_t vol;
    uint64_t notional;
};
static const uint32_t NSYM = 8;
static const uint32_t BAD_SYM = 5;
static Instrument *book[NSYM];

static void book_init() {
    static Instrument store[NSYM];
    for (uint32_t i = 0; i < NSYM; i++) {
        if (i == BAD_SYM) { book[i] = nullptr; continue; }   // the instrument is not set up
        std::snprintf(store[i].name, sizeof store[i].name, "SYM%u", i);
        store[i].vol = store[i].notional = 0;
        book[i] = &store[i];
    }
}

// The compute part of the processing: deliberately the most expensive function in the
// program — it is the one that must come out on top of the sampling profile.
static uint64_t hot_mix(uint64_t x, int rounds) {
    for (int i = 0; i < rounds; i++) {
        x ^= x >> 33;
        x *= 0xff51afd7ed558ccdull;
        x ^= x >> 29;
        x *= 0xc4ceb9fe1a85ec53ull;
    }
    return x;
}

// Booking a trade. The dereference of book[sym] here is the frame the debugger will
// show: the error is not in it but in whoever sent a non-existent instrument.
static void apply_trade(uint32_t sym, uint32_t px, uint64_t mix) {
    Instrument *ins = book[sym];
    ins->vol      += 1;
    ins->notional += (uint64_t)px + (mix & 0xff);
}

// ---- the stage queue -------------------------------------------------------

class Stage {
public:
    void push(const Tick &t) {
        {
            std::lock_guard<std::mutex> l(mu_);
            q_.push_back(t);
            if (q_.size() > depth_max_) depth_max_ = q_.size();
        }
        cv_.notify_one();
    }
    bool pop(Tick &out) {
        std::unique_lock<std::mutex> l(mu_);
        cv_.wait(l, [&] { return !q_.empty() || done_; });
        if (q_.empty()) return false;
        out = q_.front();
        q_.pop_front();
        return true;
    }
    void close() {
        { std::lock_guard<std::mutex> l(mu_); done_ = true; }
        cv_.notify_all();
    }
    size_t depth_max() const { return depth_max_; }

private:
    std::mutex              mu_;
    std::condition_variable cv_;
    std::deque<Tick>        q_;
    bool                    done_ = false;
    size_t                  depth_max_ = 0;
};

// ---- percentiles -----------------------------------------------------------

struct Samples {
    std::vector<uint32_t> v;     // stage latencies, ns
    void add(uint64_t ns) { if (v.size() < 4000000) v.push_back((uint32_t)ns); }
};

static uint32_t pct(std::vector<uint32_t> &s, double p) {
    if (s.empty()) return 0;
    size_t i = (size_t)(p * (double)(s.size() - 1));
    return s[i];
}

// ---- threads ---------------------------------------------------------------

static Stage                 stage;
static std::atomic<uint64_t> processed{0};
static std::atomic<uint64_t> written{0};
static std::atomic<bool>     stop_flag{false};
static Stage                 sink;                 // the second stage: to disk
static std::vector<Samples>  lat;                  // per worker thread
static int                   g_rounds  = 24;
// A clock running backwards is not a "negative latency" but a broken instrument:
// such samples are counted separately and never enter the percentiles.
static std::atomic<uint64_t> back_cnt{0};
static std::atomic<uint64_t> back_max{0};
static long                  g_crash_at = -1;

static void worker(int id) {
    char nm[16];
    std::snprintf(nm, sizeof nm, "md_work%d", id);
    prctl(PR_SET_NAME, nm, 0, 0, 0);

    Tick t;
    while (stage.pop(t)) {
        uint64_t mix = hot_mix(t.seq * 0x9e3779b97f4a7c15ull + t.px, g_rounds);
        uint32_t sym = t.sym;
        // A "bad" tick: the source sent an instrument that is not in the book.
        if (g_crash_at >= 0 && t.seq == (uint64_t)g_crash_at) sym = BAD_SYM;
        apply_trade(sym, t.px, mix);

        uint64_t t_done = now_ns();
        if (t_done < t.t_enq) {
            uint64_t d = t.t_enq - t_done;
            back_cnt.fetch_add(1);
            uint64_t prev = back_max.load();
            while (d > prev && !back_max.compare_exchange_weak(prev, d)) {}
        } else {
            lat[id].add(t_done - t.t_enq);
        }
        if ((processed.fetch_add(1) % 4096) == 0) sink.push(t);
    }
}

static void sinker(const char *path) {
    prctl(PR_SET_NAME, "md_sink", 0, 0, 0);
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { std::printf("mdpipe: cannot open %s (errno=%d)\n", path, errno); return; }
    Tick t;
    while (sink.pop(t)) {
        char line[96];
        int n = std::snprintf(line, sizeof line, "seq=%llu sym=%u px=%u\n",
                              (unsigned long long)t.seq, t.sym, t.px);
        if (write(fd, line, (size_t)n) == n) written.fetch_add(1);
        fsync(fd);                      // persistence: ext2 + the journal
    }
    fsync(fd);
    close(fd);
}

static void generator(uint64_t rate, uint64_t secs) {
    prctl(PR_SET_NAME, "md_gen", 0, 0, 0);
    const uint64_t step = 1000000000ull / (rate ? rate : 1);
    const uint64_t t0 = now_ns();
    const uint64_t t_end = t0 + secs * 1000000000ull;
    uint64_t seq = 0;
    while (!stop_flag.load(std::memory_order_relaxed)) {
        uint64_t due = t0 + seq * step;
        uint64_t now = now_ns();
        if (now >= t_end) break;
        if (due > now) {
            struct timespec ts = { (time_t)((due - now) / 1000000000ull),
                                   (long)((due - now) % 1000000000ull) };
            nanosleep(&ts, nullptr);
        }
        Tick t;
        t.seq   = seq;
        t.t_enq = now_ns();
        t.sym   = (uint32_t)(seq % NSYM);
        if (t.sym == BAD_SYM) t.sym = 0;            // the normal stream goes around the hole
        t.px    = (uint32_t)(10000 + (seq * 7919) % 500);
        stage.push(t);
        seq++;
    }
    stage.close();
}

int main(int argc, char **argv) {
    uint64_t secs = 10, rate = 20000;
    int workers = 2;
    const char *out = "/mdpipe.log";
    for (int i = 1; i < argc; i++) {
        std::string a = argv[i];
        auto val = [&](const char *def) -> const char * { return (i + 1 < argc) ? argv[++i] : def; };
        if      (a == "--secs")     secs    = strtoull(val("10"), nullptr, 10);
        else if (a == "--rate")     rate    = strtoull(val("20000"), nullptr, 10);
        else if (a == "--workers")  workers = atoi(val("2"));
        else if (a == "--rounds")   g_rounds = atoi(val("24"));
        else if (a == "--out")      out     = val("/mdpipe.log");
        else if (a == "--crash-at") g_crash_at = atol(val("-1"));
    }
    if (workers < 1) workers = 1;

    prctl(PR_SET_NAME, "md_main", 0, 0, 0);
    book_init();
    lat.resize((size_t)workers);

    std::printf("mdpipe: start — %d threads, rate %llu ticks/s, %llu s, file %s\n",
                workers, (unsigned long long)rate, (unsigned long long)secs, out);
    if (g_crash_at >= 0)
        std::printf("mdpipe: at tick %ld an instrument outside the book will be sent\n", g_crash_at);
    std::fflush(stdout);

    std::thread ts(sinker, out);
    std::vector<std::thread> ws;
    for (int i = 0; i < workers; i++) ws.emplace_back(worker, i);
    std::thread tg(generator, rate, secs);

    // The main thread is the workload's control plane: once a second it prints
    // what is going on. This is what is visible in serial even without the operator console.
    uint64_t prev = 0;
    for (uint64_t s = 1; s <= secs; s++) {
        struct timespec one = {1, 0};
        nanosleep(&one, nullptr);
        uint64_t p = processed.load();
        std::printf("mdpipe: %llu s — processed %llu (+%llu/s), queue max %zu, records %llu\n",
                    (unsigned long long)s, (unsigned long long)p,
                    (unsigned long long)(p - prev), stage.depth_max(),
                    (unsigned long long)written.load());
        std::fflush(stdout);
        prev = p;
    }
    stop_flag.store(true);

    tg.join();
    for (auto &w : ws) w.join();
    sink.close();
    ts.join();

    std::vector<uint32_t> all;
    for (auto &s : lat) all.insert(all.end(), s.v.begin(), s.v.end());
    std::sort(all.begin(), all.end());
    std::printf("mdpipe: total — ticks %llu, samples %zu, clock went backwards %llu times (max %llu ns)\n",
                (unsigned long long)processed.load(), all.size(),
                (unsigned long long)back_cnt.load(), (unsigned long long)back_max.load());
    std::printf("mdpipe: stage latency, ns: p50=%u p90=%u p99=%u p99.9=%u max=%u\n",
                pct(all, 0.50), pct(all, 0.90), pct(all, 0.99), pct(all, 0.999),
                all.empty() ? 0 : all.back());
    std::printf("mdpipe: OK\n");
    std::fflush(stdout);
    return 0;
}
