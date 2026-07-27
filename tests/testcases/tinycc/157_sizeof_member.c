#include <stdio.h>

struct Foo {
    int x;
    char name[1024];
    int y;
};

struct Foo *global_ptr;

int main() {
    printf("direct: %d\n", (int)sizeof(((struct Foo *)0)->name));
    printf("global_ptr: %d\n", (int)sizeof global_ptr->name);

    struct Foo local;
    struct Foo *p = &local;
    printf("local_ptr: %d\n", (int)sizeof p->name);
    printf("local_direct: %d\n", (int)sizeof local.name);

    /* Test sizeof used in array size declaration (the TCC bug pattern) */
    char buf1[sizeof p->name];
    printf("buf_from_ptr: %d\n", (int)sizeof buf1);

    char buf2[sizeof global_ptr->name];
    printf("buf_from_global: %d\n", (int)sizeof buf2);

    char buf3[sizeof local.name];
    printf("buf_from_direct: %d\n", (int)sizeof buf3);

    return 0;
}
