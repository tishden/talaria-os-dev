#include <unistd.h>
int main(void) {
    const char m[] = "musl static binary -> Talaria OS!\n";
    write(1, m, sizeof(m) - 1);
    return 5;
}
