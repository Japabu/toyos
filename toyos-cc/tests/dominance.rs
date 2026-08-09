//! One case per producer of a local's address, in the shape that used to make
//! the Cranelift verifier reject the function.
//!
//! A `Value` may only be used in blocks its defining instruction dominates.
//! Caching the address of a local as a `Value` therefore compiles for as long
//! as control reaches the variable only from below its declaration, and stops
//! the compiler dead the first time a `goto` or a loop reaches it from
//! anywhere else. Every case here refused to compile before the storage type
//! named the *origin* of each address instead of one block's answer.
//!
//! The fourth producer — the copy an aggregate parameter is given — is not
//! here because it cannot fail: that copy is cut in the entry block, which
//! dominates the whole function.

mod common;

use common::accepts;

#[test]
fn a_goto_can_reach_past_an_aggregate_declaration() {
    accepts(
        "struct S { int a; int b; };
         int f(void) {
             goto start;
             {
                 struct S s;
                 int arr[4];
             start:
                 arr[0] = 1;
                 s.a = 2;
                 return arr[0] + s.a;
             }
         }",
    );
}

#[test]
fn a_static_local_is_readable_from_a_block_that_does_not_dominate_it() {
    accepts(
        "int f(int c) {
             if (c) goto start;
             {
                 static int s = 5;
             start:
                 return s;
             }
         }",
    );
}

#[test]
fn a_vla_declared_in_a_loop_body_is_freed_after_the_loop() {
    accepts(
        "int f(int n) {
             int i;
             for (i = 0; i < 10; i++) {
                 int v[n];
                 v[0] = i;
             }
             return 0;
         }",
    );
}

#[test]
fn a_goto_can_reach_past_a_vla() {
    accepts(
        "int f(int n) {
             goto start;
             {
                 int v[n];
             start:
                 return n;
             }
         }",
    );
}

/// Not a dominance question, and here because it is the same shape of blind
/// spot: a type asked about an expression from outside the block that declares
/// its names. `expr_type` answered a statement expression without entering it,
/// so every local the block declared was an unknown identifier.
#[test]
fn a_statement_expression_types_its_own_locals() {
    accepts("int f(void) { return ({ int i = 1; i + 2; }) + 1; }");
    accepts("long g(void) { return ({ long *p = 0; p; }) == 0; }");
    accepts("int h(void) { return ({ int i = 1; ({ int j = i; j + 1; }); }); }");
}

/// `continue` is the way `79_vla_continue` reaches it, and it leaves the loop
/// through a different edge from the one above.
#[test]
fn a_vla_survives_a_continue() {
    accepts(
        "int f(int n) {
             int i, total = 0;
             for (i = 0; i < 10; i++) {
                 int v[n];
                 v[0] = i;
                 if (i & 1) continue;
                 total += v[0];
             }
             return total;
         }",
    );
}
