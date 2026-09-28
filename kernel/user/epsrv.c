#include <sys/socket.h>
#include <sys/epoll.h>
#include <netinet/in.h>
#include <unistd.h>
#include <string.h>
#include <fcntl.h>

int main(void) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET; a.sin_port = htons(80); a.sin_addr.s_addr = 0;
    bind(s, (struct sockaddr*)&a, sizeof(a));
    listen(s, 16);
    fcntl(s, F_SETFL, fcntl(s, F_GETFL, 0) | O_NONBLOCK); // the listener is non-blocking (an accept4 loop until EAGAIN, as on Linux)

    int ep = epoll_create1(0);
    struct epoll_event ev = {0};
    ev.events = EPOLLIN; ev.data.fd = s;
    epoll_ctl(ep, EPOLL_CTL_ADD, s, &ev);

    const char *r =
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n"
        "Connection: keep-alive\r\nContent-Length: 33\r\n\r\n"
        "Hello from Talaria epoll server!\n";

    struct epoll_event evs[16];
    for (;;) {
        int n = epoll_wait(ep, evs, 16, -1);
        for (int i = 0; i < n; i++) {
            int fd = evs[i].data.fd;
            if (fd == s) {
                int c;
                while ((c = accept4(s, 0, 0, SOCK_NONBLOCK)) >= 0) {
                    struct epoll_event ce = {0};
                    ce.events = EPOLLIN; ce.data.fd = c;
                    epoll_ctl(ep, EPOLL_CTL_ADD, c, &ce);
                }
            } else {
                char buf[1024];
                int n = read(fd, buf, sizeof(buf));
                if (n <= 0) { // the client has closed -> remove it and close
                    epoll_ctl(ep, EPOLL_CTL_DEL, fd, 0);
                    close(fd);
                } else {
                    // keep-alive: answer every complete request (the boundary \r\n\r\n)
                    int reqs = 0;
                    for (int i = 0; i + 3 < n; i++)
                        if (buf[i]=='\r'&&buf[i+1]=='\n'&&buf[i+2]=='\r'&&buf[i+3]=='\n') reqs++;
                    if (reqs == 0) reqs = 1;
                    for (int k = 0; k < reqs; k++) write(fd, r, strlen(r));
                    // the connection stays open and in the epoll
                }
            }
        }
    }
}
