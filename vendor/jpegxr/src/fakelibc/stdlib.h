#include <stddef.h>

typedef long long ssize_t;
typedef unsigned long long uint64_t;
typedef unsigned int uint32_t;
typedef unsigned short uint16_t;
typedef double double_t;
typedef unsigned long uintptr_t;
typedef int FILE;

#define UCHAR_MAX 255

#define getenv(NAME) 0

long long int strtoll (const char* str, char** endptr, int base);
void qsort(void *, size_t, size_t, int (*)(const void *, const void *));
char *strstr(const char *h, const char *n);
void abort(void);

double frexp(double x, int *e);
double ldexp(double x, int n);

int abs(int a);

int fclose(FILE *f);
int printf(const char *restrict fmt, ...);
int fseek(FILE *f, long off, int whence);
long ftell(FILE *f);
size_t fread(void *restrict destv, size_t size, size_t nmemb, FILE *restrict f);
int feof(FILE *f);
FILE *fopen(const char *restrict filename, const char *restrict mode);
int tolower(int c);
char *strncpy(char *restrict d, const char *restrict s, size_t n);
size_t wcslen(const wchar_t *s);