#include <stdio.h>

/* Defined in companion file 90+_static_vs_global.c */
int use_static_items(void);

/* Global "items" — an array of ints */
int items[4] = {100, 200, 300, 400};

int main() {
    printf("%d\n", items[0]);
    int ok = use_static_items();
    printf("%d\n", ok);
    return ok ? 0 : 1;
}
