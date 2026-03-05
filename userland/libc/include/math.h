#ifndef _MATH_H
#define _MATH_H

#define HUGE_VAL (__builtin_huge_val())
#define INFINITY (__builtin_inf())
#define NAN (__builtin_nan(""))

double floor(double x);
double ceil(double x);
double sqrt(double x);
double fabs(double x);
double fmod(double x, double y);
double pow(double base, double exp);
double log(double x);
double log2(double x);
double log10(double x);
double exp(double x);
double sin(double x);
double cos(double x);
double tan(double x);
double asin(double x);
double acos(double x);
double atan(double x);
double atan2(double y, double x);
double sinh(double x);
double cosh(double x);
double tanh(double x);
double round(double x);
double trunc(double x);
float floorf(float x);
float ceilf(float x);
float sqrtf(float x);
float fabsf(float x);
double ldexp(double x, int exp);
double frexp(double x, int *exp);

int isnan(double x);
int isinf(double x);
int isfinite(double x);

#endif
