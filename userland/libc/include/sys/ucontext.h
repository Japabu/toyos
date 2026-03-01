#ifndef _SYS_UCONTEXT_H
#define _SYS_UCONTEXT_H

typedef struct ucontext {
    unsigned long uc_flags;
    struct ucontext *uc_link;
    void *uc_stack;
    /* Enough space for signal mask and mcontext on x86_64 */
    unsigned long uc_mcontext[32];
} ucontext_t;

#endif
