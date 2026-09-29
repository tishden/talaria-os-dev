// cppdtor: a repro of a #DE at the exit (RELEASE-1.0 §1.3, decoded in docs/GLOSSARY.md) — a global object
// whose destructor goes into the futex path (a thread + a mutex + a condvar) after the return from main.
// The build: musl-g++ (see cpptest2) -static -no-pie -Wl,-Ttext-segment=0x10000000
#include <cstdio>
#include <thread>
#include <mutex>
#include <condition_variable>
#include <atomic>

static std::atomic<int> phase{0};

struct Global {
    ~Global() {
        std::mutex m;
        std::condition_variable cv;
        bool done = false;
        std::thread t([&] {
            { std::lock_guard<std::mutex> l(m); done = true; }
            cv.notify_one();
        });
        {
            std::unique_lock<std::mutex> l(m);
            cv.wait(l, [&] { return done; });
        }
        t.join();
        printf("cppdtor: the global destructor with the futex path has worked\n");
    }
};
static Global g;

int main() {
    std::thread bg([] { phase.store(1); });
    bg.join();
    printf("cppdtor: main is finishing (phase=%d), the global destructors are next\n", phase.load());
    return 0;
}
