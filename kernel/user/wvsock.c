/* wvsock — a writev OVER A SOCKET must go out as ONE segment.
 *
 * Why the gate. Earlier every iovec went out as a separate `write`, that is, as a
 * separate TCP segment: nginx gives the answer out as writev(the header, the body), and
 * the client got two segments for one answer — a spare packet, a spare recv at the client
 * and a spare turnaround in the latency. The mistake is invisible functionally (the bytes
 * are the same and in the same order), which is why no test caught it: it is caught only
 * by counting the receives on the reader's side.
 *
 * The check: the server sends three pieces with one writev; the client reads and counts
 * how many times recv returned data. One means it came together into one segment.
 *
 * THE STATE: it is NOT included in the regression. Both ends in one process require a
 * connection to THEIR OWN address, and the stack has no loopback interface: a bind on
 * 0.0.0.0 goes through, a connect to 10.0.0.2 does not. To make a gate out of this, the
 * reader has to be moved out onto the host (the guest listens and writes a writev, the
 * host counts the receives) — see docs/ROADMAP.md §2.9. For now the behaviour is checked by the
 * bench/suite set: the syscalls per request for nginx 3.0 -> 2.0 and exactly one tx frame
 * per request.
 */
#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#define PORT 7391

static char A[64], B[200], C[300];

int main(void) {
    memset(A, 'a', sizeof A); memset(B, 'b', sizeof B); memset(C, 'c', sizeof C);
    const size_t total = sizeof A + sizeof B + sizeof C;

    int ls = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(ls, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    /* We listen on every address and go to OUR OWN: the stack has no separate loopback
     * interface, and 10.0.0.2 is the guest's default address (talaria.ip=). */
    struct sockaddr_in any, sa;
    memset(&any, 0, sizeof any);
    any.sin_family = AF_INET; any.sin_port = htons(PORT); any.sin_addr.s_addr = 0;
    memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET; sa.sin_port = htons(PORT);
    sa.sin_addr.s_addr = htonl(0x0a000002); /* 10.0.0.2 */
    if (bind(ls, (struct sockaddr *)&any, sizeof any) < 0) { printf("wvsock: the bind failed\n"); return 1; }
    listen(ls, 4);

    int c = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    if (connect(c, (struct sockaddr *)&sa, sizeof sa) < 0) { printf("wvsock: the connect failed\n"); return 1; }
    int s = accept(ls, NULL, NULL);
    if (s < 0) { printf("wvsock: the accept failed\n"); return 1; }
    setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);

    struct iovec iov[3] = { { A, sizeof A }, { B, sizeof B }, { C, sizeof C } };
    ssize_t w = writev(s, iov, 3);
    if (w != (ssize_t)total) { printf("wvsock: writev returned %ld instead of %zu\n", (long)w, total); return 1; }

    /* How many times the reader saw data. One receive = one segment. */
    char buf[1024];
    size_t got = 0;
    int recvs = 0;
    while (got < total) {
        ssize_t n = recv(c, buf + got, sizeof buf - got, 0);
        if (n <= 0) break;
        got += (size_t)n; recvs++;
    }
    if (got != total) { printf("wvsock: %zu of %zu have been read\n", got, total); return 1; }
    if (memcmp(buf, A, sizeof A) || memcmp(buf + sizeof A, B, sizeof B)
        || memcmp(buf + sizeof A + sizeof B, C, sizeof C)) {
        printf("wvsock: the order of the pieces has been broken\n"); return 1;
    }
    printf("wvsock: the writev(3 pieces, %zu B) has been assembled, the reader's receives %d\n", total, recvs);
    if (recvs == 1) printf("wvsock: ONE segment — OK\n");
    else printf("wvsock: more than one segment (%d receives) — the writev does not assemble\n", recvs);
    close(c); close(s); close(ls);
    return 0;
}
