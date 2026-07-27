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
int access(const char *path, int mode);
unsigned int sleep(unsigned int seconds);
int usleep(unsigned long usec);
int isatty(int fd);
int execvp(const char *file, char *const argv[]);
int fork(void);
int pipe(int pipefd[2]);
void _exit(int status);

pid_t getpid(void);
pid_t getppid(void);
uid_t getuid(void);
uid_t geteuid(void);
gid_t getgid(void);
gid_t getegid(void);
int kill(pid_t pid, int sig);

long sysconf(int name);

#define _SC_PAGESIZE        30
#define _SC_CLK_TCK          2
#define _SC_NPROCESSORS_ONLN 84

#define F_OK 0
#define R_OK 4
#define W_OK 2
#define X_OK 1

#endif
