/* Simulate TCC's tok_str_add2 / tok_str_add_tok pattern exactly.
   Builds a token string for the expression: __SIZEOF_POINTER__ == 4
   which in TCC becomes: LINENUM(19) CLLONG(8) EQ CLLONG(4) EOF
   Then reads it back and prints each element. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef union {
    unsigned long long i;
    int tab[4];
} CValue;

typedef struct {
    int *str;
    int len;
    int need_spc;
    int allocated_len;
    int last_line_num;
} TokStr;

static void tok_str_realloc(TokStr *s, int new_size) {
    int size = s->allocated_len;
    if (size < 16) size = 16;
    while (size < new_size) size *= 2;
    s->str = (int *)realloc(s->str, size * sizeof(int));
    s->allocated_len = size;
}

static void tok_str_add2(TokStr *s, int t, CValue *cv) {
    int len = s->len;
    if (len + 4 >= s->allocated_len)
        tok_str_realloc(s, len + 5);
    int *ptr = s->str;
    ptr[len++] = t;
    switch (t) {
    case 0xc2: /* TOK_CINT */
    case 0xc3: /* TOK_CUINT */
    case 0xcf: /* TOK_LINENUM */
        ptr[len++] = cv->tab[0];
        break;
    case 0xc4: /* TOK_CLLONG */
    case 0xc5: /* TOK_CULLONG */
        ptr[len++] = cv->tab[0];
        ptr[len++] = cv->tab[1];
        break;
    default:
        break;
    }
    s->len = len;
}

/* Mirror tok_str_add_tok: adds LINENUM if line changed, then current tok */
static void tok_str_add_tok(TokStr *s, int tok, CValue *tokc, int line_num) {
    CValue lv;
    if (line_num != s->last_line_num) {
        s->last_line_num = line_num;
        lv.i = (unsigned long long)line_num;
        tok_str_add2(s, 0xcf, &lv);
    }
    tok_str_add2(s, tok, tokc);
}

int main(void) {
    TokStr s;
    CValue cv;
    int i;

    memset(&s, 0, sizeof(s));
    s.last_line_num = -1;

    /* Simulate building "#if __SIZEOF_POINTER__ == 4" token string */
    /* __SIZEOF_POINTER__ expands to 8 as CLLONG on 64-bit */
    cv.i = 8ULL;
    tok_str_add_tok(&s, 0xc4, &cv, 19);  /* CLLONG(8), line 19 */

    cv.i = 0;
    tok_str_add_tok(&s, 0x94, &cv, 19);  /* EQ (==) */

    cv.i = 4ULL;
    tok_str_add_tok(&s, 0xc4, &cv, 19);  /* CLLONG(4) */

    /* EOF marker */
    s.str[s.len++] = -1;

    printf("len=%d\n", s.len);
    for (i = 0; i < s.len; i++) {
        printf("[%d]=%d\n", i, s.str[i]);
    }

    /* Also verify specific positions */
    printf("tok[0](linenum)=%d\n", s.str[0] == 0xcf ? 1 : 0);
    printf("tok[1](line19)=%d\n",  s.str[1] == 19   ? 1 : 0);
    printf("tok[2](cllong)=%d\n",  s.str[2] == 0xc4 ? 1 : 0);
    printf("tok[3](8)=%d\n",       s.str[3] == 8    ? 1 : 0);
    printf("tok[4](0)=%d\n",       s.str[4] == 0    ? 1 : 0);
    printf("tok[5](eq)=%d\n",      s.str[5] == 0x94 ? 1 : 0);
    printf("tok[6](cllong)=%d\n",  s.str[6] == 0xc4 ? 1 : 0);
    printf("tok[7](4)=%d\n",       s.str[7] == 4    ? 1 : 0);
    printf("tok[8](0)=%d\n",       s.str[8] == 0    ? 1 : 0);
    printf("tok[9](eof)=%d\n",     s.str[9] == -1   ? 1 : 0);

    free(s.str);
    return 0;
}
