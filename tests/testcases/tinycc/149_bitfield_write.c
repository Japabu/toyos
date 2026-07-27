extern int printf(const char *, ...);

struct F {
    unsigned func_call : 3;
    unsigned func_type : 2;
    unsigned func_noreturn : 1;
    unsigned func_ctor : 1;
    unsigned func_dtor : 1;
};

/* Test basic bitfield writes */
void test_basic(void) {
    struct F f;
    f.func_call = 0;
    f.func_type = 0;
    f.func_noreturn = 0;
    f.func_ctor = 0;
    f.func_dtor = 0;

    f.func_type = 3;
    printf("func_type = %u\n", f.func_type);
    printf("func_call = %u\n", f.func_call);

    f.func_call = 5;
    printf("func_call = %u\n", f.func_call);
    printf("func_type = %u\n", f.func_type);

    f.func_noreturn = 1;
    printf("func_noreturn = %u\n", f.func_noreturn);
    printf("func_type = %u\n", f.func_type);
    printf("func_call = %u\n", f.func_call);
}

/* Test compound assignment to bitfields */
void test_compound(void) {
    struct F f;
    f.func_call = 0;
    f.func_type = 0;
    f.func_noreturn = 0;
    f.func_ctor = 0;
    f.func_dtor = 0;

    f.func_call = 2;
    f.func_call += 1;
    printf("compound += : %u\n", f.func_call);

    f.func_type = 1;
    f.func_type |= 2;
    printf("compound |= : %u\n", f.func_type);
}

/* Test that writes don't clobber neighbors */
void test_no_clobber(void) {
    struct F f;
    f.func_call = 7;
    f.func_type = 3;
    f.func_noreturn = 1;
    f.func_ctor = 1;
    f.func_dtor = 1;

    f.func_type = 0;
    printf("after clear type: call=%u type=%u noreturn=%u ctor=%u dtor=%u\n",
           f.func_call, f.func_type, f.func_noreturn, f.func_ctor, f.func_dtor);

    f.func_call = 0;
    printf("after clear call: call=%u type=%u noreturn=%u ctor=%u dtor=%u\n",
           f.func_call, f.func_type, f.func_noreturn, f.func_ctor, f.func_dtor);
}

int main(void) {
    test_basic();
    test_compound();
    test_no_clobber();
    return 0;
}
