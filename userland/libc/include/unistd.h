#ifndef _UNISTD_H
#define _UNISTD_H

#include <stddef.h>
#include <sys/types.h>

#define STDIN_FILENO  0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

ssize_t read(int fd, void *buf, size_t count);
ssize_t write(int fd, const void *buf, size_t count);
int close(int fd);
off_t lseek(int fd, off_t offset, int whence);
int dup(int oldfd);
int dup2(int oldfd, int newfd);
int unlink(const char *path);
int rmdir(const char *path);
char *getcwd(char *buf, size_t size);
int chdir(const char *path);
int access(int fd, int mode);
unsigned int sleep(unsigned int seconds);
int usleep(unsigned long usec);
int isatty(int fd);
int execvp(const char *file, char *const argv[]);
int fork(void);
int pipe(int pipefd[2]);
long sysconf(int name);

#define _SC_PAGESIZE 30

#endif
