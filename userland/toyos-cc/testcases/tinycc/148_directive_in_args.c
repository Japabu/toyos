extern int printf(const char*, ...);
extern unsigned long strlen(const char*);

/* Test preprocessor directives inside function argument lists.
   This pattern appears in TCC where #if/#include is used within
   a function call to conditionally include string literals. */

#define USE_EMBEDDED 1

void print_len(const char *s, int extra) {
    printf("%d\n", (int)strlen(s) + extra);
}

int main() {
    /* Directive inside function args — the text before #if must not be dropped */
    print_len(
#if USE_EMBEDDED
        "hello"
#else
        "hi"
#endif
        , 0);

    /* Multiple string literals from conditional */
    print_len(
#if USE_EMBEDDED
        "world"
#else
        "w"
#endif
        , 100);

    return 0;
}
