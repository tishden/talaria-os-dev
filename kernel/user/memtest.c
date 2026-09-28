#include <sys/mman.h>
#include <stdio.h>
#include <string.h>
int main(void){
    // 2000 cycles of mmap 1MB + munmap — without reclaim it would exhaust the allocator
    long ok=0;
    for(int i=0;i<2000;i++){
        void* p=mmap(0,1<<20,PROT_READ|PROT_WRITE,MAP_ANONYMOUS|MAP_PRIVATE,-1,0);
        if(p==MAP_FAILED){ printf("MEMTEST: the mmap failed at the iteration %d (there is no reclaim)\n",i); return 1; }
        memset(p,0xAB,4096); // touch it
        munmap(p,1<<20);
        ok++;
    }
    printf("MEMTEST: %ld cycles of mmap(1MB)+munmap without exhaustion -> the reclaim works\n",ok);
    return 0;
}
