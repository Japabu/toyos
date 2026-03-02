#ifndef _FLOAT_H
#define _FLOAT_H

#define FLT_RADIX       2
#define FLT_MANT_DIG    24
#define DBL_MANT_DIG    53
#define LDBL_MANT_DIG   53

#define FLT_MIN_EXP     (-125)
#define DBL_MIN_EXP     (-1021)
#define LDBL_MIN_EXP    (-1021)
#define FLT_MAX_EXP     128
#define DBL_MAX_EXP     1024
#define LDBL_MAX_EXP    1024

#define FLT_MIN         1.17549435e-38F
#define FLT_MAX         3.40282347e+38F
#define FLT_EPSILON     1.19209290e-7F
#define FLT_DIG         6
#define FLT_MIN_10_EXP  (-37)
#define FLT_MAX_10_EXP  38

#define DBL_MIN         2.2250738585072014e-308
#define DBL_MAX         1.7976931348623157e+308
#define DBL_EPSILON     2.2204460492503131e-16
#define DBL_DIG         15
#define DBL_MIN_10_EXP  (-307)
#define DBL_MAX_10_EXP  308

/* long double == double on all supported targets */
#define LDBL_MIN        2.2250738585072014e-308L
#define LDBL_MAX        1.7976931348623157e+308L
#define LDBL_EPSILON    2.2204460492503131e-16L
#define LDBL_DIG        15
#define LDBL_MIN_10_EXP (-307)
#define LDBL_MAX_10_EXP 308

/* Compiler built-in float functions as pure-C macros */
#define __builtin_inf()    (1.0/0.0)
#define __builtin_inff()   (1.0F/0.0F)
#define __builtin_infl()   (1.0L/0.0L)
#define __builtin_fabs(x)  ((x)<0?-(x):(x))
#define __builtin_fabsf(x) ((x)<0?-(x):(x))
#define __builtin_fabsl(x) ((x)<0?-(x):(x))

#define HUGE_VAL   (1.0/0.0)
#define HUGE_VALF  (1.0F/0.0F)
#define HUGE_VALL  (1.0L/0.0L)
#define INFINITY   (1.0F/0.0F)
#define NAN        (0.0F/0.0F)

#endif /* _FLOAT_H */
