#ifndef _ASSERT_H
#define _ASSERT_H

#ifdef NDEBUG
#define assert(x) ((void)0)
#else
#define assert(x) ((x) ? ((void)0) : __builtin_trap())
#endif

#endif
