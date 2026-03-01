typedef unsigned char u8;
struct SS {u8 a[3], b; };
struct SS sinit16[] = { { 1 }, 2 };
struct S
{
  u8 a,b;
  u8 c[2];
};

struct T
{
  u8 s[16];
  u8 a;
};

struct U
{
  u8 a;
  struct S s;
  u8 b;
  struct T t;
};

struct V
{
  struct S s;
  struct T t;
  u8 a;
};

struct W
{
  struct V t;
  struct S s[];
};

struct S gs = ((struct S){1, 2, 3, 4});
struct S gs2 = {1, 2, {3, 4}};
struct T gt = {"hello", 42};
struct U gu = {3, 5,6,7,8, 4, "huhu", 43};
struct U gu2 = {3, {5,6,7,8}, 4, {"huhu", 43}};
/* Optional braces around scalar initializers.  Accepted, but with
   a warning.  */
struct U gu3 = { {3}, {5,6,7,8,}, 4, {"huhu", 43}};
/* Many superfluous braces and leaving out one initializer for U.s.c[1] */
struct U gu4 = { 3, {5,6,7,},  5, { "bla", {44}} };
/* Superfluous braces and useless parens around values */
struct S gs3 = { (1), {(2)}, {(((3))), {4}}};
/* Superfluous braces, and leaving out braces for V.t, plus cast */
struct V gv = {{{3},4,{5,6}}, "haha", (u8)45, 46};
/* Compound literal */
struct V gv2 = {(struct S){7,8,{9,10}}, {"hihi", 47}, 48};
/* Parens around compound literal */
struct V gv3 = {((struct S){7,8,{9,10}}), {"hoho", 49}, 50};
/* Initialization of a flex array member (warns in GCC) */
struct W gw = {{1,2,3,4}, {1,2,3,4,5}};

union UU {
    u8 a;
    u8 b;
};
struct SU {
    union UU u;
    u8 c;
};
struct SU gsu = {5,6};

struct in6_addr {
    union {
	u8 u6_addr8[16];
	unsigned short u6_addr16[8];
    } u;
};
struct flowi6 {
    struct in6_addr saddr, daddr;
};
struct pkthdr {
    struct in6_addr daddr, saddr;
};
struct pkthdr phdr = { { { 6,5,4,3 } }, { { 9,8,7,6 } } };

struct Wrap {
    void *func;
};
int global;
void inc_global (void)
{
  global++;
}

struct Wrap global_wrap[] = {
    ((struct Wrap) {inc_global}),
    inc_global,
};

#include <stdio.h>
void print_ (const char *name, const u8 *p, long size)
{
  printf ("%s:", name);
  while (size--) {
      printf (" %x", *p++);
  }
  printf ("\n");
}
#define print(x) print_(#x, (u8*)&x, sizeof (x))

void foo (struct W *w, struct pkthdr *phdr_)
{
  struct S ls = {1, 2, 3, 4};
  struct S ls2 = {1, 2, {3, 4}};
  struct T lt = {"hello", 42};
  struct U lu = {3, 5,6,7,8, 4, "huhu", 43};
  struct U lu1 = {3, ls, 4, {"huhu", 43}};
  struct U lu2 = {3, (ls), 4, {"huhu", 43}};
  const struct S *pls = &ls;
  struct S ls21 = *pls;
  struct U lu22 = {3, *pls, 4, {"huhu", 43}};
  /* Incomplete bracing.  */
  struct U lu21 = {3, ls, 4, "huhu", 43};
  /* Optional braces around scalar initializers.  Accepted, but with
     a warning.  */
  struct U lu3 = { 3, {5,6,7,8,}, 4, {"huhu", 43}};
  /* Many superfluous braces and leaving out one initializer for U.s.c[1] */
  struct U lu4 = { 3, {5,6,7,},  5, { "bla", 44} };
  /* Superfluous braces and useless parens around values */
  struct S ls3 = { (1), (2), {(((3))), 4}};
  /* Superfluous braces, and leaving out braces for V.t, plus cast */
  struct V lv = {{3,4,{5,6}}, "haha", (u8)45, 46};
  /* Compound literal */
  struct V lv2 = {(struct S)w->t.s, {"hihi", 47}, 48};
  /* Parens around compound literal */
  struct V lv3 = {((struct S){7,8,{9,10}}), ((const struct W *)w)->t.t, 50};
  const struct pkthdr *phdr = phdr_;
  struct flowi6 flow = { .daddr = phdr->daddr, .saddr = phdr->saddr };
  struct S ls4 = {.a = 1, .b = 2, .c = {3, 4}};
  print(ls);
  print(ls2);
  print(lt);
  print(lu);
  print(lu1);
  print(lu2);
  print(ls21);
  print(lu21);
  print(lu22);
  print(lu3);
  print(lu4);
  print(ls3);
  print(lv);
  print(lv2);
  print(lv3);
  print(flow);
  print(ls4);
}

void test_compound_with_relocs (void)
{
  struct Wrap local_wrap[] = {
      ((struct Wrap) {inc_global}),
      inc_global,
  };
  void (*p)(void);
  p = global_wrap[0].func; p();
  p = global_wrap[1].func; p();
  p = local_wrap[0].func; p();
  p = local_wrap[1].func; p();
}

/* Following is from GCC gcc.c-torture/execute/20050613-1.c.  */

struct SEA { int i; int j; int k; int l; };
struct SEB { struct SEA a; int r[1]; };
struct SEC { struct SEA a; int r[0]; };
struct SED { struct SEA a; int r[]; };

static void
test_correct_filling (struct SEA *x)
{
  static int i;
  if (x->i != 0 || x->j != 5 || x->k != 0 || x->l != 0)
    printf("sea_fill%d: wrong\n", i);
  else
    printf("sea_fill%d: okay\n", i);
  i++;
}

int
test_zero_init (void)
{
  /* The peculiarity here is that only a.j is initialized.  That
     means that all other members must be zero initialized.  TCC
     once didn't do that for sub-level designators.  */
  struct SEB b = { .a.j = 5 };
  struct SEC c = { .a.j = 5 };
  struct SED d = { .a.j = 5 };
  test_correct_filling (&b.a);
  test_correct_filling (&c.a);
  test_correct_filling (&d.a);
  return 0;
}

void test_init_struct_from_struct(void)
{
    int i = 0;
    struct S {int x,y;}
        a = {1,2},
        b = {3,4},
        c[] = {a,b},
        d[] = {++i, ++i, ++i, ++i},
        e[] = {b, (struct S){5,6}}
        ;

    printf("%s: %d %d %d %d - %d %d %d %d - %d %d %d %d\n",
        __func__,
        c[0].x,
        c[0].y,
        c[1].x,
        c[1].y,
        d[0].x,
        d[0].y,
        d[1].x,
        d[1].y,
        e[0].x,
        e[0].y,
        e[1].x,
        e[1].y
        );
}

typedef struct {
    unsigned int a;
    unsigned int : 32;
    unsigned int b;
    unsigned long long : 64;
    unsigned int c;
} tst_bf;

tst_bf arr[] = { { 1, 2, 3 } };

void
test_init_bf(void)
{
    printf ("%s: %d %d %d\n", __func__, arr[0].a, arr[0].b, arr[0].c);
}


int main()
{
  print(gs);
  print(gs2);
  print(gt);
  print(gu);
  print(gu2);
  print(gu3);
  print(gu4);
  print(gs3);
  print(gv);
  print(gv2);
  print(gv3);
  print(sinit16);
  print(gw);
  print(gsu);
  print(phdr);
  foo(&gw, &phdr);
  test_compound_with_relocs();
  test_zero_init();
  test_init_struct_from_struct();
  test_init_bf();
  return 0;
}
