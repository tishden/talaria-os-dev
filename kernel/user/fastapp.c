/* A zero-syscall UDP echo: it polls the SPSC rings in the shared memory and reflects the
   packet. The only syscalls are musl's start + fastmap + the banner. There are NO syscalls in the loop. */
#include <stdint.h>
#include <string.h>
#include <unistd.h>

#define FN 256
#define FSLOT 2048
#define HDR 16
#define SLOT_SZ (HDR + FSLOT)
#define RING_SZ (128 + FN * SLOT_SZ)
#define RX_OFF 0
#define TX_OFF RING_SZ

static inline volatile uint32_t* PROD(char* b, long r){ return (volatile uint32_t*)(b + r); }
static inline volatile uint32_t* CONS(char* b, long r){ return (volatile uint32_t*)(b + r + 64); }
static inline char* SLOT(char* b, long r, uint32_t i){ return b + r + 128 + (uint64_t)(i & (FN-1)) * SLOT_SZ; }

static inline uint64_t fastmap(void){
    uint64_t ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(1000UL) : "rcx","r11","memory");
    return ret;
}

static void swap(uint8_t* a, uint8_t* b, int n){ for(int i=0;i<n;i++){ uint8_t t=a[i]; a[i]=b[i]; b[i]=t; } }

static void reflect(uint8_t* p, uint32_t len){
    if (len < 14) return;
    swap(p, p+6, 6);                       // eth dst<->src
    if (p[12]==0x08 && p[13]==0x00 && len>=34){ // IPv4
        swap(p+26, p+30, 4);               // ip src<->dst
        if (p[23]==17 && len>=42){          // UDP
            swap(p+34, p+36, 2);           // the udp src<->dst port
            p[40]=0; p[41]=0;              // the udp checksum = 0 (lawful for IPv4)
        }
    }
}

int main(void){
    char* b = (char*)fastmap();
    const char* m = "fastapp: the zero-syscall echo is up (ring 3, the polling of the rings)\n";
    write(1, m, strlen(m));
    uint32_t rc = *CONS(b, RX_OFF);
    for(;;){
        uint32_t rp = *PROD(b, RX_OFF);
        while (rc != rp){
            char* rs = SLOT(b, RX_OFF, rc);
            uint32_t len = *(volatile uint32_t*)rs;
            if (len > FSLOT) len = FSLOT;
            uint32_t tp = *PROD(b, TX_OFF);
            if (tp - *CONS(b, TX_OFF) < FN){
                char* ts = SLOT(b, TX_OFF, tp);
                memcpy(ts + HDR, rs + HDR, len);
                reflect((uint8_t*)(ts + HDR), len);
                *(volatile uint32_t*)ts = len;
                __asm__ volatile("" ::: "memory");
                *PROD(b, TX_OFF) = tp + 1;
            }
            rc++;
            *CONS(b, RX_OFF) = rc;
        }
    }
}
