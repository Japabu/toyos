#ifndef _TOYOS_CC_COMPAT_H
#define _TOYOS_CC_COMPAT_H

/* Language standard */
#define __STDC__ 1
#define __STDC_VERSION__ 199901L

/* Architecture — derived from compiler seed (__aarch64__ or __x86_64__) */
#define __LP64__ 1
#define __SIZEOF_POINTER__ 8
#define __SIZEOF_LONG__ 8
#define __SIZEOF_INT__ 4
#define __SIZEOF_SHORT__ 2
#define __SIZEOF_LONG_LONG__ 8
#define __CHAR_BIT__ 8

#ifdef __aarch64__
#define __arm64__ 1
#define __ARM_64BIT_STATE 1
#else
#define __x86_64 1
#define __amd64__ 1
#define __amd64 1
#endif

/* OS — derived from compiler seed (__TOYOS__, __APPLE__, or __linux__) */
#ifdef __TOYOS__
#define __unix__ 1
#define __ELF__ 1
#elif defined __APPLE__
#define __APPLE_CC__ 1
#define __MACH__ 1
#elif defined __linux__
#define __unix__ 1
#define __ELF__ 1
#define __gnu_linux__ 1
#endif

/* Built-in type macros (LP64) */
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

/* Compiler attributes — not supported, strip them */
#define __attribute__(x)
#define __attribute(x)
#define __declspec(x)

/* Feature predicates — C99 only, we support nothing */
#define __has_builtin(x) 0
#define __has_feature(x) 0
#define __has_attribute(x) 0
#define __has_extension(x) 0

/* Nullability qualifiers — not supported, strip them */
#define _Nonnull
#define _Nullable
#define _Nullable_result
#define _Null_unspecified
#define __nonnull
#define __nullable
#define __null_unspecified

/* GCC misc */
#define __PRETTY_FUNCTION__ __FUNCTION__

#endif /* _TOYOS_CC_COMPAT_H */
