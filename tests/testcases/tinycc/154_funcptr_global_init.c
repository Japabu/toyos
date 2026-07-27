#include <stdio.h>

/* Forward declarations only — functions defined after the global initializer */
void hello(void);
void world(void);

typedef void (*func_t)(void);

/* Function pointers in global initializers (forward-declared functions) */
func_t funcs[] = { hello, world };
void (*single)(void) = hello;

int main() {
    funcs[0]();
    funcs[1]();
    single();
    return 0;
}

void hello(void) { printf("hello\n"); }
void world(void) { printf("world\n"); }
