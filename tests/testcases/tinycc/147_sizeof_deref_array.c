extern int printf(const char*, ...);

/* Test sizeof applied to dereferenced array.
   sizeof(*arr) where arr is an array should give the element size.
   This pattern appears in TCC: sizeof(libs) / sizeof(*libs) */

int main() {
    int arr[5] = {1, 2, 3, 4, 5};
    const char *strs[] = {"hello", "world", 0};

    /* sizeof array / sizeof element = number of elements */
    printf("%d\n", (int)(sizeof(arr) / sizeof(*arr)));
    printf("%d\n", (int)(sizeof(strs) / sizeof(*strs)));

    /* sizeof(*arr) == sizeof(int) */
    printf("%d\n", (int)sizeof(*arr));

    /* sizeof(*strs) == sizeof(char*) */
    printf("%d\n", (int)sizeof(*strs));

    return 0;
}
