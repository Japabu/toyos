/* Test uint64_t split into two ints and reassembly.
   Tests the pattern used for TOK_CLLONG in tok_str_add2:
     ptr[len++] = cv->i;
     ptr[len++] = cv->i >> 32; */
#include <stdio.h>
#include <stdlib.h>

typedef unsigned long long u64;

static void split_store(int *arr, int idx, u64 val) {
    arr[idx]     = (int)(val & 0xffffffff);
    arr[idx + 1] = (int)(val >> 32);
}

static u64 split_load(int *arr, int idx) {
    return (u64)(unsigned int)arr[idx] | ((u64)(unsigned int)arr[idx + 1] << 32);
}

/* Store via CValue union (as TCC does) */
typedef union { u64 i; int tab[4]; } CValue;

static void cval_store(int *arr, int idx, CValue *cv) {
    arr[idx]     = cv->tab[0];
    arr[idx + 1] = cv->tab[1];
}

static u64 cval_load(int *arr, int idx) {
    CValue cv;
    cv.tab[0] = arr[idx];
    cv.tab[1] = arr[idx + 1];
    return cv.i;
}

int main(void) {
    int arr[16] = {0};
    u64 vals[] = {0ULL, 1ULL, 4ULL, 8ULL, 0x100000000ULL,
                  0xdeadbeefcafeULL, 0xffffffffffffffffULL};
    int n = sizeof(vals) / sizeof(vals[0]);
    int i;

    for (i = 0; i < n; i++) {
        u64 v = vals[i];
        split_store(arr, 0, v);
        u64 r = split_load(arr, 0);
        printf("split: %s\n", r == v ? "ok" : "FAIL");
    }

    for (i = 0; i < n; i++) {
        CValue cv;
        cv.i = vals[i];
        cval_store(arr, 0, &cv);
        u64 r = cval_load(arr, 0);
        printf("cval: %s\n", r == vals[i] ? "ok" : "FAIL");
    }

    /* Test with computed index (as in tok_str_add2 where len is runtime) */
    int len = 0;
    CValue cv;
    cv.i = 8ULL;
    arr[len++] = 0xc4;       /* token type */
    arr[len++] = cv.tab[0]; /* low 32 bits */
    arr[len++] = cv.tab[1]; /* high 32 bits */
    printf("tok=%d val=%llu\n", arr[0], cval_load(arr, 1));

    return 0;
}
