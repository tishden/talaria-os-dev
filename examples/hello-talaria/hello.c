// hello-talaria: a minimal application for Talaria OS (C, static musl).
// Shows printf/stdio, arguments from cell.manifest, reading a file from disk and
// threads (clone/futex through pthread). Built by the Makefile, packed by talaria-pack.
#include <stdio.h>
#include <string.h>
#include <pthread.h>

static long counter = 0;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;

static void *worker(void *arg) {
    (void)arg;
    for (int i = 0; i < 100000; i++) {
        pthread_mutex_lock(&mu);
        counter++;
        pthread_mutex_unlock(&mu);
    }
    return NULL;
}

int main(int argc, char **argv) {
    printf("hello-talaria: hello from ring 3! argc=%d argv[1]=%s\n",
           argc, argc > 1 ? argv[1] : "(none)");

    FILE *f = fopen("/greeting.txt", "r");
    if (f) {
        char line[128] = {0};
        if (fgets(line, sizeof line, f)) {
            line[strcspn(line, "\n")] = 0;
            printf("hello-talaria: from disk: %s\n", line);
        }
        fclose(f);
    } else {
        printf("hello-talaria: /greeting.txt not found\n");
    }

    pthread_t a, b;
    pthread_create(&a, NULL, worker, NULL);
    pthread_create(&b, NULL, worker, NULL);
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    printf("hello-talaria: threads joined, counter=%ld (expected 200000)\n", counter);
    printf("hello-talaria: OK\n");
    return 0;
}
