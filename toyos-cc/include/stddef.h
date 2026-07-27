#ifndef _STDDEF_H
#define _STDDEF_H

typedef __SIZE_TYPE__    size_t;
typedef __PTRDIFF_TYPE__ ptrdiff_t;
typedef __WCHAR_TYPE__   wchar_t;

#ifndef NULL
#define NULL ((void*)0)
#endif

#undef offsetof
#define offsetof(type, field) __builtin_offsetof(type, field)

#endif /* _STDDEF_H */
