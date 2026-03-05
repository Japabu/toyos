#include <stdio.h>

struct item { int x; int y; };

struct item items[] = { {1,2}, {3,4}, {5,6} };
int count = sizeof(items) / sizeof(*items);
int elem_size = sizeof(items[0]);

int main() {
    printf("%d\n", count);
    printf("%d\n", elem_size);
    return 0;
}
