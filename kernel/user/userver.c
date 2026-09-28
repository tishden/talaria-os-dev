/* A musl UDP echo server: recvfrom/sendto through the socket syscalls -> smoltcp.
   A pair of syscalls per packet — the opposite of the zero-syscall fastpath. */
#include <sys/socket.h>
#include <netinet/in.h>
#include <string.h>
#include <unistd.h>
int main(void){
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET; a.sin_port = htons(9998); a.sin_addr.s_addr = 0;
    bind(s, (struct sockaddr*)&a, sizeof(a));
    const char* m = "userver: the UDP echo through the socket syscalls is up\n";
    write(1, m, strlen(m));
    char buf[2048];
    for(;;){
        struct sockaddr_in src; socklen_t sl = sizeof(src);
        int n = recvfrom(s, buf, sizeof(buf), 0, (struct sockaddr*)&src, &sl);
        if (n > 0) sendto(s, buf, n, 0, (struct sockaddr*)&src, sl);
    }
}
