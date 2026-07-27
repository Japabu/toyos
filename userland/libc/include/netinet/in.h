#ifndef _NETINET_IN_H
#define _NETINET_IN_H

#include <stdint.h>
#include <sys/socket.h>

#define INADDR_ANY       ((uint32_t)0x00000000)
#define INADDR_LOOPBACK  ((uint32_t)0x7f000001)
#define INADDR_NONE      ((uint32_t)0xffffffff)

#define INET_ADDRSTRLEN  16

#define TCP_NODELAY 1

struct in_addr {
    uint32_t s_addr;
};

struct sockaddr_in {
    unsigned short sin_family;
    uint16_t       sin_port;
    struct in_addr sin_addr;
    char           sin_zero[8];
};

uint16_t htons(uint16_t hostshort);
uint16_t ntohs(uint16_t netshort);
uint32_t htonl(uint32_t hostlong);
uint32_t ntohl(uint32_t netlong);

#endif
