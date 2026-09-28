#include <stdio.h>
#include <stdlib.h>
int main(void) {
    char *b = malloc(64);
    for (int i = 0; i < 3; i++) printf("Talaria: the line %d, malloc=%p\n", i, (void*)b);
    free(b);
    return 0;
}
