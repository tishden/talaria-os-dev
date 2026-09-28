// hello-talaria (C++): STL, exceptions, std::thread + condition_variable —
// the full C++ runtime on Talaria OS. Static musl build (Alpine g++).
#include <cstdio>
#include <map>
#include <string>
#include <thread>
#include <mutex>
#include <condition_variable>

int main() {
    std::map<std::string, int> m{{"talaria", 1}, {"os", 2}};
    try { throw std::string("an exception"); }
    catch (const std::string &e) { std::printf("hellopp: caught %s\n", e.c_str()); }

    std::mutex mu;
    std::condition_variable cv;
    bool ready = false;
    int sum = 0;
    std::thread t([&] {
        { std::lock_guard<std::mutex> l(mu); for (auto &kv : m) sum += kv.second; ready = true; }
        cv.notify_one();
    });
    {
        std::unique_lock<std::mutex> l(mu);
        cv.wait(l, [&] { return ready; });
    }
    t.join();
    std::printf("hellopp: map sum from a thread = %d, OK\n", sum);
    return 0;
}
