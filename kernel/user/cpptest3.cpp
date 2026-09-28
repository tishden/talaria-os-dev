// cpptest3: iostream + std::map + sort (the regression talaria.test=cpp3 asserts
// "iostream+map+sort: talaria=3"). The source has been restored from the binary's
// behaviour (RELEASE-1.0 §4.6). The build: see the Makefile.
#include <iostream>
#include <map>
#include <string>
#include <vector>
#include <algorithm>

int main() {
    std::map<std::string, int> m;
    m["talaria"] = 3; m["os"] = 2; m["cpp"] = 1;
    std::vector<int> v{5, 3, 1, 4, 2};
    std::sort(v.begin(), v.end());
    std::cout << "iostream+map+sort: talaria=" << m["talaria"] << " sorted=";
    for (int x : v) std::cout << x;
    std::cout << " map_size=" << m.size() << std::endl;
    return 0;
}
