#include <stdio.h>

int arr[] = {10, 20, 30, 40, 50};
int *p = &arr[2];

int main() {
    printf("%d\n", *p);
    printf("%d\n", p[1]);
    return 0;
}
