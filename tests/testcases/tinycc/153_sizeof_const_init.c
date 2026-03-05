#include <stdio.h>

/* sizeof in global initializer constant expressions */
int len1 = sizeof("hello") - 1;
int len2 = sizeof("iddt") - 1;
int sz_int = sizeof(int);
int sz_ptr = sizeof(void*);

int main() {
    printf("%d\n", len1);
    printf("%d\n", len2);
    printf("%d\n", sz_int);
    printf("%d\n", sz_ptr);
    return 0;
}
