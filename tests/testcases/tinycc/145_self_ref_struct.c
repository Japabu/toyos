extern int printf(const char*, ...);
extern void *malloc(unsigned long);

/* Self-referential struct: the 'next' field is a pointer to the same struct.
   When resolving the struct definition, 'next' gets an incomplete type because
   the struct isn't registered yet. Field access through 'next' must resolve
   the incomplete type to find fields like 'value'. */

typedef struct Node {
    int value;
    int extra;
    struct Node *next;
} Node;

/* Also test arrow through a chain: a->next->next->value */
int main() {
    Node c = { 30, 300, 0 };
    Node b = { 20, 200, &c };
    Node a = { 10, 100, &b };

    /* Direct access */
    printf("%d\n", a.value);

    /* One level of indirection via self-referential pointer */
    printf("%d\n", a.next->value);
    printf("%d\n", a.next->extra);

    /* Two levels of indirection */
    printf("%d\n", a.next->next->value);
    printf("%d\n", a.next->next->extra);

    /* Allocate dynamically and access through pointer */
    Node *p = malloc(sizeof(Node));
    p->value = 42;
    p->extra = 420;
    p->next = &a;
    printf("%d\n", p->value);
    printf("%d\n", p->next->value);
    printf("%d\n", p->next->next->value);

    return 0;
}
