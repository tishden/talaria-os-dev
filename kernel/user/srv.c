#include <sys/socket.h>
#include <netinet/in.h>
#include <unistd.h>
#include <string.h>
int main(void) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(80);
    a.sin_addr.s_addr = 0;
    bind(s, (struct sockaddr*)&a, sizeof(a));
    listen(s, 8);
    for (;;) {
        int c = accept(s, 0, 0);
        if (c < 0) continue;
        char req[512];
        read(c, req, sizeof(req));
        const char *r = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: 40\r\n\r\nHello from Talaria userspace server!!!\r\n";
        write(c, r, strlen(r));
        close(c);
    }
}
