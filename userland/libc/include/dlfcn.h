#ifndef _DLFCN_H
#define _DLFCN_H

#define RTLD_LAZY   0x1
#define RTLD_NOW    0x2
#define RTLD_GLOBAL 0x100
#define RTLD_LOCAL  0x000
#define RTLD_DEFAULT ((void *)0)

void *dlopen(const char *filename, int flags);
void *dlsym(void *handle, const char *symbol);
int dlclose(void *handle);
char *dlerror(void);

#endif
