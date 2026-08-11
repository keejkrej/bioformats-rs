#ifndef FAKELIBC_TIME_H
#define FAKELIBC_TIME_H

typedef long time_t;
typedef long clock_t;

struct tm {
    int tm_year;
    int tm_mon;
    int tm_mday;
    int tm_hour;
    int tm_min;
    int tm_sec;
    int tm_isdst;
};

struct timespec {
    time_t tv_sec;
    time_t tv_nsec;
};

#define CLOCKS_PER_SEC 1000000

#endif