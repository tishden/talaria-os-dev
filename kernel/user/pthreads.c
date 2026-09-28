#include <pthread.h>
#include <stdio.h>
static volatile long counter = 0;
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
#define NT 4
#define ITER 50000
static void* worker(void* arg){
    long id = (long)arg;
    for (int i = 0; i < ITER; i++){
        pthread_mutex_lock(&m);
        counter++;
        pthread_mutex_unlock(&m);
    }
    printf("  the thread %ld has finished %d increments\n", id, ITER);
    return 0;
}
int main(void){
    pthread_t t[NT];
    for (long i = 0; i < NT; i++) pthread_create(&t[i], 0, worker, (void*)i);
    for (int i = 0; i < NT; i++) pthread_join(t[i], 0);
    printf("PTHREADS: counter=%ld (we expected %d) -> %s\n",
        counter, NT*ITER, counter == NT*ITER ? "OK" : "A DIFFERENCE");
    return 0;
}
