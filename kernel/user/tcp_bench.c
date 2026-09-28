/* A keep-alive bench: N requests over one TCP connection, req/sec. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <time.h>
#include <arpa/inet.h>
#include <sys/socket.h>
static uint64_t nowns(void){ struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return (uint64_t)t.tv_sec*1000000000ULL+t.tv_nsec; }
int main(int argc,char**argv){
    const char* ip=argc>1?argv[1]:"10.0.0.2"; int port=argc>2?atoi(argv[2]):80; int N=argc>3?atoi(argv[3]):20000;
    int s=socket(AF_INET,SOCK_STREAM,0);
    struct sockaddr_in a={0}; a.sin_family=AF_INET; a.sin_port=htons(port); inet_pton(AF_INET,ip,&a.sin_addr);
    if(connect(s,(void*)&a,sizeof a)){ printf("connect fail\n"); return 1; }
    const char* req="GET / HTTP/1.1\r\nHost: t\r\n\r\n";
    char buf[512]; int ok=0; uint64_t t0=nowns();
    for(int i=0;i<N;i++){
        if(write(s,req,strlen(req))<=0) break;
        int r=read(s,buf,sizeof buf);
        if(r>0 && (buf[9]=='2')) ok++; // "HTTP/1.1 2xx"
    }
    uint64_t t1=nowns(); double sec=(t1-t0)/1e9;
    printf("TCP-BENCH keep-alive: %d/%d answers in %.3f s -> %.0f req/sec, %.1f us/request\n",
        ok,N,sec, ok/sec, sec*1e6/ok);
    return 0;
}
