/* Test #include inside a multi-line function call (unbalanced parens).
   This pattern is used by TCC to embed tccdefs_.h as string arguments. */
#define USE_EMBEDDED 1
foo(bar,
#if USE_EMBEDDED
    #include "24_include_in_call.h"
#else
    "fallback\n"
#endif
    , -1);
