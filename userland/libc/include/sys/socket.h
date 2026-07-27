#ifndef _SYS_SOCKET_H
#define _SYS_SOCKET_H

#include <stddef.h>
#include <sys/types.h>

typedef unsigned int socklen_t;

#define AF_UNSPEC 0
#define AF_INET   2

#define SOCK_STREAM 1
#define SOCK_DGRAM  2

#define IPPROTO_TCP 6
#define IPPROTO_UDP 17

#define SOL_SOCKET  1
#define SO_REUSEADDR 2
#define SO_ERROR     4
#define SO_KEEPALIVE 9
#define SO_RCVTIMEO  20
#define SO_SNDTIMEO  21

#define SHUT_RD   0
#define SHUT_WR   1
#define SHUT_RDWR 2

#define MSG_NOSIGNAL 0x4000

struct sockaddr {
    unsigned short sa_family;
    char sa_data[14];
};

struct sockaddr_storage {
    unsigned short ss_family;
    char _pad[126];
};

int socket(int domain, int type, int protocol);
int connect(int fd, const struct sockaddr *addr, socklen_t addrlen);
int bind(int fd, const struct sockaddr *addr, socklen_t addrlen);
int listen(int fd, int backlog);
int accept(int fd, struct sockaddr *addr, socklen_t *addrlen);
ssize_t send(int fd, const void *buf, size_t len, int flags);
ssize_t recv(int fd, void *buf, size_t len, int flags);
ssize_t sendto(int fd, const void *buf, size_t len, int flags,
               const struct sockaddr *dest_addr, socklen_t addrlen);
ssize_t recvfrom(int fd, void *buf, size_t len, int flags,
                 struct sockaddr *src_addr, socklen_t *addrlen);
int shutdown(int fd, int how);
int setsockopt(int fd, int level, int optname, const void *optval, socklen_t optlen);
int getsockopt(int fd, int level, int optname, void *optval, socklen_t *optlen);
int getpeername(int fd, struct sockaddr *addr, socklen_t *addrlen);
int getsockname(int fd, struct sockaddr *addr, socklen_t *addrlen);

#endif
