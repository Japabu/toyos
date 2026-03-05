#include <stdio.h>
#include <stddef.h>

struct Foo {
    int a;
    char b;
    long c;
    short d;
};

struct Nested {
    int x;
    struct Foo foo;
};

// Classic offsetof pattern used by TCC and others:
// (size_t)&((struct Type*)0)->field
#define MY_OFFSETOF(type, field) ((size_t)&((type *)0)->field)

// Use offsetof in a global initializer (this is the hard part — must be
// evaluated at compile time for static data)
static unsigned short offsets[] = {
    MY_OFFSETOF(struct Foo, a),
    MY_OFFSETOF(struct Foo, b),
    MY_OFFSETOF(struct Foo, c),
    MY_OFFSETOF(struct Foo, d),
};

int main(void) {
    // Runtime offsetof using the standard macro
    printf("offsetof(Foo, a) = %zu\n", offsetof(struct Foo, a));
    printf("offsetof(Foo, b) = %zu\n", offsetof(struct Foo, b));
    printf("offsetof(Foo, c) = %zu\n", offsetof(struct Foo, c));
    printf("offsetof(Foo, d) = %zu\n", offsetof(struct Foo, d));

    // Global initializer using the cast-to-null pattern
    printf("offsets: %u %u %u %u\n", offsets[0], offsets[1], offsets[2], offsets[3]);

    // Nested struct
    printf("offsetof(Nested, foo) = %zu\n", offsetof(struct Nested, foo));

    return 0;
}
