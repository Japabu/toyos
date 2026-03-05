#include <stdio.h>

typedef int fixed_t;

/* DOOM-style fixed-point: (fixed_t)(-0.867 * (1 << 16)) */
fixed_t val1 = (fixed_t)(-0.867 * (1 << 16));
fixed_t val2 = (fixed_t)(0.5 * (1 << 16));
int val3 = (int)(3.14 * 100);
int val4 = (int)(1.0 * (1 << 8));

int main() {
    printf("%d\n", val1);
    printf("%d\n", val2);
    printf("%d\n", val3);
    printf("%d\n", val4);
    return 0;
}
