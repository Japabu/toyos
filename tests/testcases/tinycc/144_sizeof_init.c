int printf(const char*, ...);
void *malloc(unsigned long);

struct Small { char a; };
struct Medium { int x; int y; double z; };
struct Large { char data[256]; };

int main()
{
    /* sizeof *ptr in declaration initializer — the exact bug pattern.
       The declared pointer's type must be visible during its own initializer. */
    struct Small *sp = malloc(sizeof *sp);
    printf("sizeof *sp: %d\n", (int)sizeof *sp);

    struct Medium *mp = malloc(sizeof *mp);
    printf("sizeof *mp: %d\n", (int)sizeof *mp);

    struct Large *lp = malloc(sizeof *lp);
    printf("sizeof *lp: %d\n", (int)sizeof *lp);

    /* sizeof(var) for scalar in own initializer */
    int i = sizeof(i);
    printf("sizeof i: %d\n", i);

    long l = sizeof(l);
    printf("sizeof l: %d\n", (int)l);

    /* sizeof applied to pointer variable itself in own initializer */
    char *cp = malloc(sizeof(cp));
    printf("sizeof cp: %d\n", (int)sizeof(cp));

    return 0;
}
