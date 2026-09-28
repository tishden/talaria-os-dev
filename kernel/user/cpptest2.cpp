// cpptest2: std::thread + mutex + atomic + chrono (a mascot is a mascot, and the
// regression talaria.test=cpp2 asserts "C++ threads: atomic=200000 guarded=200000").
// The source has been restored from the binary's behaviour (RELEASE-1.0 §4.6: every
// binary in the repo must have a source). The build: see the Makefile.
#include <cstdio>
#include <thread>
#include <mutex>
#include <atomic>
#include <chrono>

static std::atomic<int> acnt{0};
static int guarded = 0;
static std::mutex mu;

static void worker() {
    for (int i = 0; i < 100000; i++) {
        acnt.fetch_add(1, std::memory_order_relaxed);
        std::lock_guard<std::mutex> l(mu);
        guarded++;
    }
}

int main() {
    auto t0 = std::chrono::steady_clock::now();
    std::thread a(worker), b(worker);
    a.join(); b.join();
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now() - t0).count();
    std::printf("C++ threads: atomic=%d guarded=%d in %lld ms\n",
                acnt.load(), guarded, (long long)ms);
    return 0;
}
