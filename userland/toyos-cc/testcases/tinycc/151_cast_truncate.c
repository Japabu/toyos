#include <stdio.h>
#include <stdint.h>

int main() {
    // Cast signed 64-bit to unsigned 32-bit, then assign to unsigned 64-bit:
    // must truncate to 32 bits, then zero-extend
    long long x = -8;
    uint64_t y = (uint32_t)x;
    printf("y = 0x%llx\n", (unsigned long long)y);

    // Same with -28
    long long a = -28;
    uint64_t b = (uint32_t)a;
    printf("b = 0x%llx\n", (unsigned long long)b);

    // Direct use in arithmetic
    long long c = -1;
    uint64_t d = (uint32_t)c + 0;
    printf("d = 0x%llx\n", (unsigned long long)d);

    // Cast unsigned 32 to signed 64 (sign-extends)
    unsigned int u = 0xFFFFFFFF;
    long long s = (int)u;
    printf("s = %lld\n", s);

    // Cast signed to unsigned, use in comparison
    long long neg = -100;
    if ((uint32_t)neg > 100)
        printf("large unsigned: correct\n");
    else
        printf("small: wrong\n");

    return 0;
}
