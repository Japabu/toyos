/* Test #if with __SIZEOF_POINTER__ - this is the exact pattern
   that caused tcc-stage1 to crash. */
#include <stdio.h>

#if __SIZEOF_POINTER__ == 8
# define PTR_BITS 64
#elif __SIZEOF_POINTER__ == 4
# define PTR_BITS 32
#else
# error "unknown pointer size"
#endif

/* Nested #if inside an else branch - exercises the stack more */
#if __SIZEOF_POINTER__ > 4
# if __SIZEOF_POINTER__ >= 8
#  define IS_LP64 1
# else
#  define IS_LP64 0
# endif
#endif

int main(void) {
    printf("PTR_BITS=%d\n", PTR_BITS);
    printf("sizeof(void*)=%d\n", (int)sizeof(void *));
#ifdef IS_LP64
    printf("IS_LP64=%d\n", IS_LP64);
#endif
    return 0;
}
