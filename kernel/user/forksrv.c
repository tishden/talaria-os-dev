// forksrv — a server with a FORK per connection (the PostgreSQL model), plus one
// background process waiting in `poll`.
//
// WHY EXACTLY LIKE THIS. The gate catches one specific defect: the ownership of a socket
// ("it is I who wait for this descriptor") survived the CLOSING of that descriptor by its
// owner. The postmaster accepts a connection, forks a backend and closes its own copy —
// and the record about the owner stays on it. Then the addressed yield
// (`netsys::ready_peer`) sees a socket WITH DATA whose "waiter" is the postmaster, and
// gives the core to it; that one gives it to the background process, that one back, and
// the core closes into a ring for two. The forked backends get no processor AT ALL
// (the measurement: 30 tasks, zero picks by the scheduler on each).
//
// Hence the two mandatory participants, and neither is for beauty:
//   * the FORK per connection — otherwise nobody "becomes an owner by mistake";
//   * a BACKGROUND process waiting for the network (in PostgreSQL that is the
//     checkpointer on its latch) — otherwise the ring has nobody to close with, and the
//     parent would simply reach the child by the rotation.
#include <sys/socket.h>
#include <netinet/in.h>
#include <poll.h>
#include <unistd.h>
#include <string.h>
#include <stdio.h>
#include <sys/wait.h>

// How many connections to serve before exiting: the exit is needed not by the server but
// by the gate — the kernel's counters are printed at the workload's finish.
//
// THE NUMBER IS AN ARGUMENT, and that is not an ornament. Six connections are enough for
// the gate about the socket's ownership, but NOT enough for the second question: on the
// stand on the 8th of September PostgreSQL's listening socket DISAPPEARED from the table
// ("listeners 0" in `stat: tcp slots`) after 119 accepted connections, while before that
// a hundred-odd forks had not touched it. So the event is rare, and it has to be caught
// with hundreds of fork+accept+close cycles rather than with six.
#define WANT_CONNS 6

int main(int argc, char **argv) {
    int want = WANT_CONNS;
    if (argc > 1) {
        int v = 0;
        for (const char *p = argv[1]; *p >= '0' && *p <= '9'; p++) v = v * 10 + (*p - '0');
        if (v > 0) want = v;
    }
    // WHETHER TO REAP THE CHILDREN OR NOT — these are two DIFFERENT models, and both are needed.
    //
    // A real PostgreSQL postmaster does reap its children, so it does not exhaust the task
    // table; the question "where does the listener go" is posed only on this model
    // (`reap`). The model WITHOUT reaping is needed by another gate — `forkcap`: it runs
    // into the ceiling of the task table, and before the fix the kernel wrote past the
    // arrays' bounds on that and hung dead.
    int reap = (argc > 2 && argv[2][0] == 'r');
    int pfd[2];
    if (pipe(pfd) != 0) { printf("forksrv: pipe REFUSED\n"); return 1; }
    if (fork() == 0) {                       // the background process — the "checkpointer"
        close(pfd[1]);
        struct pollfd p = { pfd[0], POLLIN, 0 };
        char b;
        // It exits when the parent closes its end of the pipe: without this the set does
        // not finish, while the kernel's counters (`stat:`) are printed at the workload's
        // finish — and the gate would have nothing to read.
        for (;;) { poll(&p, 1, 60000); if (read(pfd[0], &b, 1) == 0) break; }
        _exit(0);
    }
    close(pfd[0]);

    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET; a.sin_port = htons(80); a.sin_addr.s_addr = 0;
    if (bind(s, (struct sockaddr *)&a, sizeof(a)) != 0) { printf("forksrv: bind REFUSED\n"); return 1; }
    listen(s, 16);
    printf("forksrv: a fork per connection, the port 80 (the background waiter is up)\n");
    fflush(stdout);
    int served = 0;
    for (;;) {
        int c = accept(s, 0, 0);
        if (c < 0) continue;
        if (fork() == 0) {
            close(s);
            char buf[256];
            if (read(c, buf, sizeof(buf)) > 0)
                write(c, "HTTP/1.0 200 OK\r\nContent-Length: 12\r\n\r\nFORKSRV OK\r\n", 51);
            close(c);
            _exit(0);
        }
        close(c);          // THE PARENT CLOSES ITS OWN COPY — this is where the ownership was lost
        if (reap) { int st; while (waitpid(-1, &st, WNOHANG) > 0) { } }   // like the postmaster
        if (++served >= want) {
            printf("forksrv: %d connections have been served\n", served);
            fflush(stdout);
            sleep(1);          // let the backends answer
            close(pfd[1]);     // the background one will see a POLLHUP and exit
            close(s);
            return 0;
        }
    }
}
