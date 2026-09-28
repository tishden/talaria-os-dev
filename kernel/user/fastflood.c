/* A microbench of the TX bandwidth of the zero-syscall rings: N UDP frames (a broadcast MAC,
   10.0.0.2 -> 10.0.0.255:7000) into the tx ring as fast as possible; it measures the speed at
   which the cell (an AP) takes them. It prints pkt/s. */
#include <stdint.h>
#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#define FN 256
#define FSLOT 2048
#define HDR 16
#define SLOT_SZ (HDR + FSLOT)
#define RING_SZ (128 + FN * SLOT_SZ)
#define TX_OFF RING_SZ
static inline volatile uint32_t* PROD(char* b, long r){ return (volatile uint32_t*)(b + r); }
static inline volatile uint32_t* CONS(char* b, long r){ return (volatile uint32_t*)(b + r + 64); }
static inline char* SLOT(char* b, long r, uint32_t i){ return b + r + 128 + (uint64_t)(i & (FN-1)) * SLOT_SZ; }
static inline uint64_t fastmap(void){ uint64_t ret; __asm__ volatile("syscall" : "=a"(ret) : "a"(1000UL) : "rcx","r11","memory"); return ret; }
static double now(void){ struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return t.tv_sec + t.tv_nsec/1e9; }
int main(int argc, char** argv){
    char* b = (char*)fastmap();
    int N = 100000, len = 300; if (argc > 1) N = atoi(argv[1]); if (argc > 2) len = atoi(argv[2]);
    uint8_t f[2048]; memset(f, 0, sizeof f);
    memset(f, 0xff, 6); uint8_t mac[6]={0x52,0x54,0,0x12,0x34,0x56}; memcpy(f+6, mac, 6); f[12]=8; f[13]=0;
    f[14]=0x45; uint16_t tl = len-14; f[16]=tl>>8; f[17]=tl; f[22]=64; f[23]=17; f[26]=10; f[27]=0; f[28]=0; f[29]=2; f[30]=10; f[31]=0; f[32]=0; f[33]=255;
    uint32_t s=0; for(int i=14;i<34;i+=2) s += (f[i]<<8)|f[i+1]; while(s>>16) s=(s&0xffff)+(s>>16); s=~s; f[24]=s>>8; f[25]=s;
    f[34]=0x1b; f[35]=0x58; f[36]=0x1b; f[37]=0x58; uint16_t ul=len-34; f[38]=ul>>8; f[39]=ul;
    double t0 = now(); uint32_t tp = *PROD(b, TX_OFF);
    for (int i = 0; i < N; i++) {
        while (tp - *CONS(b, TX_OFF) >= FN) { __asm__ volatile("pause"); }
        char* ts = SLOT(b, TX_OFF, tp); memcpy(ts + HDR, f, len); *(volatile uint32_t*)ts = len;
        __asm__ volatile("" ::: "memory"); *PROD(b, TX_OFF) = ++tp;
    }
    while (*CONS(b, TX_OFF) != tp) { __asm__ volatile("pause"); }
    double dt = now() - t0;
    printf("fastflood: %d frames x %d B in %.3f s = %.0f pkt/s (%.1f us/pkt)\n", N, len, dt, N/dt, dt/N*1e6);
    return 0;
}
