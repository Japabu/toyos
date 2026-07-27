#include <stdarg.h>
#include <stdio.h>
#include <string.h>

// Test 1: Pass va_list to vsnprintf (the doom pattern)
int my_vformat(char *buf, int size, const char *fmt, va_list ap) {
    return vsnprintf(buf, size, fmt, ap);
}

int my_format(char *buf, int size, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = my_vformat(buf, size, fmt, ap);
    va_end(ap);
    return r;
}

// Test 2: Multiple types through va_list
void print_args(const char *fmt, ...) {
    char buf[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    printf("%s\n", buf);
}

// Test 3: va_copy
void print_twice(const char *fmt, ...) {
    char buf1[256], buf2[256];
    va_list ap, ap2;
    va_start(ap, fmt);
    va_copy(ap2, ap);
    vsnprintf(buf1, sizeof(buf1), fmt, ap);
    vsnprintf(buf2, sizeof(buf2), fmt, ap2);
    va_end(ap);
    va_end(ap2);
    printf("%s\n", buf1);
    printf("%s\n", buf2);
}

int main() {
    char buf[256];

    // Test 1: single int
    my_format(buf, sizeof(buf), "val=%d", 42);
    printf("%s\n", buf);

    // Test 2: multiple ints
    my_format(buf, sizeof(buf), "%d+%d=%d", 1, 2, 3);
    printf("%s\n", buf);

    // Test 3: string arg
    my_format(buf, sizeof(buf), "hello %s!", "world");
    printf("%s\n", buf);

    // Test 4: mixed types
    my_format(buf, sizeof(buf), "%s=%d", "x", 99);
    printf("%s\n", buf);

    // Test 5: direct vsnprintf passthrough
    print_args("a=%d b=%d c=%d", 10, 20, 30);

    // Test 6: va_copy
    print_twice("n=%d", 7);

    return 0;
}
