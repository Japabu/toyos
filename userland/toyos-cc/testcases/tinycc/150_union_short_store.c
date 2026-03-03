extern int printf(const char *, ...);

/* Test that storing to a short member doesn't clobber adjacent short */
struct SV {
    union {
        struct { unsigned short cmp_op, cmp_r; };
        long long sym;
    };
};

void test_no_clobber(void) {
    struct SV sv;
    sv.sym = 0; /* clear all */
    sv.cmp_r = 42;
    sv.cmp_op = 7;
    printf("cmp_r = %u\n", sv.cmp_r);
    printf("cmp_op = %u\n", sv.cmp_op);
}

/* Test storing to short fields in a struct */
struct Pair {
    unsigned short a;
    unsigned short b;
};

void test_pair(void) {
    struct Pair p;
    p.a = 0;
    p.b = 0;
    p.a = 100;
    p.b = 200;
    printf("a = %u, b = %u\n", p.a, p.b);
    p.a = 300;
    printf("a = %u, b = %u\n", p.a, p.b);
}

/* Test char fields */
struct Bytes {
    unsigned char x;
    unsigned char y;
    unsigned char z;
    unsigned char w;
};

void test_bytes(void) {
    struct Bytes b;
    b.x = 1; b.y = 2; b.z = 3; b.w = 4;
    printf("x=%u y=%u z=%u w=%u\n", b.x, b.y, b.z, b.w);
    b.y = 99;
    printf("x=%u y=%u z=%u w=%u\n", b.x, b.y, b.z, b.w);
}

int main(void) {
    test_no_clobber();
    test_pair();
    test_bytes();
    return 0;
}
