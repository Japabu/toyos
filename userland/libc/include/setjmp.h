#ifndef _SETJMP_H
#define _SETJMP_H

/* x86-64: rbx, rbp, r12-r15, rsp, rip = 8 registers */
typedef long jmp_buf[8];

int setjmp(jmp_buf env);
void longjmp(jmp_buf env, int val);

#endif
