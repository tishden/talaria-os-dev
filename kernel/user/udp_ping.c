/* A host-side meter of the RTT: N ping-pongs to a UDP echo, the percentiles in ns. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <time.h>
#include <sys/time.h>
#include <arpa/inet.h>
#include <sys/socket.h>

static int cmp(const void*a,const void*b){ uint64_t x=*(const uint64_t*)a,y=*(const uint64_t*)b; return x<y?-1:x>y?1:0; }
static uint64_t nowns(void){ struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return (uint64_t)t.tv_sec*1000000000ULL+t.tv_nsec; }

int main(int argc, char** argv){
    const char* ip = argc>1?argv[1]:"10.0.0.2";
    int port = argc>2?atoi(argv[2]):9999;
    int N = argc>3?atoi(argv[3]):50000;
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a={0}; a.sin_family=AF_INET; a.sin_port=htons(port); inet_pton(AF_INET,ip,&a.sin_addr);
    connect(s,(void*)&a,sizeof a);
    struct timeval tv={1,0}; setsockopt(s,SOL_SOCKET,SO_RCVTIMEO,&tv,sizeof tv);
    char buf[64]; uint64_t* rtt = malloc((size_t)N*8); int ok=0; int consec=0;
    for(int i=0;i<N;i++){
        memcpy(buf,"PINGPING",8);
        uint64_t t0=nowns();
        if(send(s,buf,8,0)!=8) continue;
        int r=recv(s,buf,sizeof buf,0);
        uint64_t t1=nowns();
        if(r>0){ rtt[ok++]=t1-t0; consec=0; }
        else if(++consec>50){ printf("UDP-PING %s:%d has been interrupted: 50 timeouts in a row (the target is unreachable), ok=%d\n", ip, port, ok); if(ok<10) return 1; break; }
    }
    if(ok<10){ printf("UDP-PING %s:%d — not enough answers (%d/%d)\n",ip,port,ok,N); return 1; }
    qsort(rtt,ok,8,cmp);
    uint64_t sum=0; for(int i=0;i<ok;i++) sum+=rtt[i];
    printf("UDP-PING %s:%d ok=%d/%d  RTT ns: min=%llu p50=%llu p99=%llu p999=%llu max=%llu avg=%llu\n",
        ip,port,ok,N,
        (unsigned long long)rtt[0],(unsigned long long)rtt[ok/2],
        (unsigned long long)rtt[(int)(ok*0.99)],(unsigned long long)rtt[(int)(ok*0.999)],
        (unsigned long long)rtt[ok-1],(unsigned long long)(sum/ok));
    return 0;
}
