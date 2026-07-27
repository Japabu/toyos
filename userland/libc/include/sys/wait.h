#ifndef _SYS_WAIT_H
#define _SYS_WAIT_H

#include <sys/types.h>

#define WNOHANG   1
#define WUNTRACED 2

#define WEXITSTATUS(s) (((s) >> 8) & 0xff)
#define WTERMSIG(s)    ((s) & 0x7f)
#define WIFEXITED(s)   (WTERMSIG(s) == 0)
#define WIFSIGNALED(s) (WTERMSIG(s) != 0)

pid_t wait(int *status);
pid_t waitpid(pid_t pid, int *status, int options);

#endif
