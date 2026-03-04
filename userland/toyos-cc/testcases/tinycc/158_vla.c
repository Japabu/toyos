#include <stdio.h>
#include <string.h>

void test_basic(int n) {
    char buf[n];
    memset(buf, 'A', n);
    buf[n - 1] = '\0';
    printf("basic: len=%d first=%c\n", (int)strlen(buf), buf[0]);
}

void test_sizeof(int n) {
    int arr[n];
    printf("sizeof: %d\n", (int)sizeof arr);
}

int main() {
    test_basic(10);
    test_sizeof(5);
    test_sizeof(20);
    return 0;
}
