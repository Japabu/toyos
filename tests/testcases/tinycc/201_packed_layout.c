/* Offsets and sizes here were taken from clang for x86_64-unknown-linux-gnu
   and aarch64-unknown-linux-gnu; both agree. */
#include <stdio.h>
#include <stddef.h>

struct Plain { char a; int b; char c; short d; };
struct __attribute__((packed)) Packed { char a; int b; char c; short d; };

struct __attribute__((packed)) Wide { char a; long long b; char *p; double d; };

typedef struct { char a; int b; } __attribute__((packed)) Trailing;

struct __attribute__((packed)) Inner { char a; int b; };
struct Outer { char x; struct Inner in; int y; };

union __attribute__((packed)) U { char a; int b; };

struct __attribute__((aligned(16))) Al { int a; };
struct __attribute__((packed, aligned(4))) PackAl { char a; int b; char c; };

struct __attribute__((packed)) Arr { char a; int b; };

int main(void) {
    int i;

    printf("Plain   %zu %zu %zu %zu / %zu\n",
        offsetof(struct Plain, a), offsetof(struct Plain, b),
        offsetof(struct Plain, c), offsetof(struct Plain, d), sizeof(struct Plain));
    printf("Packed  %zu %zu %zu %zu / %zu\n",
        offsetof(struct Packed, a), offsetof(struct Packed, b),
        offsetof(struct Packed, c), offsetof(struct Packed, d), sizeof(struct Packed));
    printf("Wide    %zu %zu %zu %zu / %zu\n",
        offsetof(struct Wide, a), offsetof(struct Wide, b),
        offsetof(struct Wide, p), offsetof(struct Wide, d), sizeof(struct Wide));
    printf("Trail   %zu %zu / %zu\n",
        offsetof(Trailing, a), offsetof(Trailing, b), sizeof(Trailing));
    printf("Outer   %zu %zu %zu / %zu\n",
        offsetof(struct Outer, x), offsetof(struct Outer, in),
        offsetof(struct Outer, y), sizeof(struct Outer));
    printf("Union   %zu\n", sizeof(union U));
    printf("Al      %zu\n", sizeof(struct Al));
    printf("PackAl  %zu %zu %zu / %zu\n",
        offsetof(struct PackAl, a), offsetof(struct PackAl, b),
        offsetof(struct PackAl, c), sizeof(struct PackAl));

    /* Read a byte image back through the fields, so codegen has to use the
       packed offsets and not only sizeof. */
    {
        struct Packed p;
        unsigned char *raw = (unsigned char *)&p;
        for (i = 0; i < (int)sizeof p; i++)
            raw[i] = (unsigned char)(0x11 * (i + 1));
        printf("read    %02x %08x %02x %04x\n",
            (unsigned)(unsigned char)p.a, (unsigned)p.b,
            (unsigned)(unsigned char)p.c, (unsigned)(unsigned short)p.d);
    }

    {
        struct Arr arr[3];
        arr[0].b = 1; arr[1].b = 2; arr[2].b = 3;
        printf("stride  %zu %ld %ld\n", sizeof(struct Arr),
            (long)((char *)&arr[1] - (char *)&arr[0]),
            (long)(arr[0].b + arr[1].b + arr[2].b));
    }
    return 0;
}
