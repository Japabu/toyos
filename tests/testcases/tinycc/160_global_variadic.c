#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>

char *global_ptr;

// Variadic function that reads its first (non-variadic) arg
char *join(const char *s, ...) {
    va_list args;
    const char *v;
    size_t len = strlen(s) + 1;

    va_start(args, s);
    for (;;) {
        v = va_arg(args, const char *);
        if (v == NULL) break;
        len += strlen(v);
    }
    va_end(args);

    char *result = malloc(len);
    strcpy(result, s);

    va_start(args, s);
    for (;;) {
        v = va_arg(args, const char *);
        if (v == NULL) break;
        strcat(result, v);
    }
    va_end(args);

    return result;
}

int main(void) {
    global_ptr = malloc(2);
    global_ptr[0] = '.';
    global_ptr[1] = '\0';

    printf("global_ptr=%s\n", global_ptr);

    // Pass global pointer as first arg to variadic function
    char *result = join(global_ptr, "/config", NULL);
    printf("result=%s\n", result);

    free(result);
    free(global_ptr);
    return 0;
}
