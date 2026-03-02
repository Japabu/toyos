#ifndef _TOYOS_CC_COMPAT_H
#define _TOYOS_CC_COMPAT_H

/* GCC/Clang built-in type macros (x86-64 LP64) */
#define __SIZE_TYPE__       long unsigned int
#define __PTRDIFF_TYPE__    long int
#define __WCHAR_TYPE__      int
#define __WINT_TYPE__       int
#define __INT8_TYPE__       signed char
#define __INT16_TYPE__      short
#define __INT32_TYPE__      int
#define __INT64_TYPE__      long long int
#define __UINT8_TYPE__      unsigned char
#define __UINT16_TYPE__     unsigned short
#define __UINT32_TYPE__     unsigned int
#define __UINT64_TYPE__     long long unsigned int
#define __INTPTR_TYPE__     long int
#define __UINTPTR_TYPE__    long unsigned int
#define __INTMAX_TYPE__     long int
#define __UINTMAX_TYPE__    long unsigned int

/* Compiler attributes — not supported by toyos-cc, strip them */
#define __attribute__(x)
#define __attribute(x)
#define __declspec(x)

/* Apple nullability qualifiers */
#ifdef __APPLE__
#define _Nonnull
#define _Nullable
#define _Nullable_result
#define _Null_unspecified
#define __nonnull
#define __nullable
#define __null_unspecified
#endif

#endif /* _TOYOS_CC_COMPAT_H */
