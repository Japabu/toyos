// Shadow header: fix doomgeneric's boolean typedef.
// doomgeneric uses `typedef unsigned char boolean` when __bool_true_false_are_defined
// is set (via stdbool.h). Doom's code casts boolean* to int* in the status bar,
// which only works if boolean is int-sized. Chocolate Doom fixed this upstream;
// doomgeneric never picked it up. We override the typedef here.
// See: https://blog.svgames.pl/article/the-little-bool-of-doom

#ifndef __DOOMTYPE__
#define __DOOMTYPE__

#include <strings.h>
#include <stdbool.h>
#include <inttypes.h>
#include <limits.h>

#ifdef __GNUC__
#define PACKEDATTR __attribute__((packed))
#else
#define PACKEDATTR
#endif

// Doom assumes boolean is int-sized (casts boolean* to int*).
typedef int boolean;

typedef uint8_t byte;

#define DIR_SEPARATOR '/'
#define DIR_SEPARATOR_S "/"
#define PATH_SEPARATOR ':'

#define arrlen(array) (sizeof(array) / sizeof(*array))

#endif
