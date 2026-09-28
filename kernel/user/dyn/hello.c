#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <dlfcn.h>
static void *thr(void *a) { return (void *)(long)(strlen((char *)a) * 2); }
int main(int argc, char **argv) {
  printf("dyn-hello: argc=%d argv0=%s malloc=%p\n", argc, argv[0], malloc(100));
  pthread_t t; void *r; pthread_create(&t, 0, thr, "talaria"); pthread_join(t, &r);
  printf("dyn-hello: thread result=%ld\n", (long)r);
  void *h = dlopen("libz.so.1", RTLD_NOW); printf("dyn-hello: dlopen libz=%s\n", h ? "ok" : dlerror());
  if (h) { const char *(*v)(void) = dlsym(h, "zlibVersion"); printf("dyn-hello: zlibVersion=%s\n", v ? v() : "?"); }
  puts("dyn-hello: ALL OK");
  return 0;
}
