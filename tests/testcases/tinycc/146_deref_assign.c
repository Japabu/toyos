extern int printf(const char*, ...);

/* Test dereferencing assignment expressions: *(p = &x)
   In C, assignment returns the value of the left-hand side,
   so the result has the type of the lhs. Dereferencing that
   should work correctly. This pattern appears in TCC's
   tal_free_impl: al = *(pal = &al->next) */

struct Node {
    int value;
    struct Node *next;
};

int main() {
    struct Node c = { 30, 0 };
    struct Node b = { 20, &c };
    struct Node a = { 10, &b };

    struct Node *al = &a;
    struct Node **pal = &al;

    /* Simple: deref of assignment */
    int x = 5, y = 10;
    int *p;
    printf("%d\n", *(p = &x));
    printf("%d\n", *(p = &y));

    /* The TCC pattern: al = *(pal = &al->next) */
    al = *(pal = &al->next);
    printf("%d\n", al->value);

    al = *(pal = &al->next);
    printf("%d\n", al->value);

    return 0;
}
