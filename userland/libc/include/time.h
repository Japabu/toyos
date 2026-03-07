#ifndef _TIME_H
#define _TIME_H

#include <stddef.h>

typedef long time_t;
typedef long clock_t;
typedef long suseconds_t;

struct tm {
    int tm_sec;
    int tm_min;
    int tm_hour;
    int tm_mday;
    int tm_mon;
    int tm_year;
    int tm_wday;
    int tm_yday;
    int tm_isdst;
};

struct timespec {
    time_t tv_sec;
    long   tv_nsec;
};

#define CLOCKS_PER_SEC 1000000L

#define CLOCK_REALTIME  0
#define CLOCK_MONOTONIC 1

time_t time(time_t *t);
clock_t clock(void);
int clock_gettime(int clk_id, struct timespec *tp);
int nanosleep(const struct timespec *req, struct timespec *rem);

struct tm *localtime(const time_t *timer);
struct tm *localtime_r(const time_t *timer, struct tm *result);
struct tm *gmtime(const time_t *timer);
struct tm *gmtime_r(const time_t *timer, struct tm *result);
time_t mktime(struct tm *tm);
double difftime(time_t t1, time_t t0);
size_t strftime(char *s, size_t max, const char *fmt, const struct tm *tm);

unsigned int sleep(unsigned int seconds);
int usleep(unsigned int usec);

#endif
