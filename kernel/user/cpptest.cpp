#include <cstdio>
#include <vector>
#include <string>
#include <numeric>
struct Global { Global(){ std::printf("C++ static-init ctor ran\n"); } } g;
static int throw_and_catch() {
    try { throw std::string("boom"); }
    catch (const std::string& e) { return (int)e.size(); }
}
int main() {
    std::vector<int> v;
    for (int i = 1; i <= 10; ++i) v.push_back(i * i);
    long sum = std::accumulate(v.begin(), v.end(), 0L);
    std::string s = "Talaria runs C++! sum(sq 1..10)=";
    std::printf("%s%ld, vec.size=%zu, catch=%d\n", s.c_str(), sum, v.size(), throw_and_catch());
    return 0;
}
