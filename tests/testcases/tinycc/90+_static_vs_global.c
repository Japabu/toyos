#include <stdio.h>

/* This file has a static variable named "items" — same name as the global
   in the main file, but different type. The linker must keep them separate. */
static int a[] = {10, 20, 30};
static int b[] = {40, 50, 60};
static int *items[] = {a, b};

int use_static_items(void) {
    /* Access through the static "items" pointer array */
    int *p0 = items[0];
    int *p1 = items[1];
    printf("%d %d %d\n", p0[0], p0[1], p0[2]);
    printf("%d %d %d\n", p1[0], p1[1], p1[2]);
    /* Verify values are correct */
    return (p0[0] == 10 && p0[1] == 20 && p0[2] == 30 &&
            p1[0] == 40 && p1[1] == 50 && p1[2] == 60);
}
