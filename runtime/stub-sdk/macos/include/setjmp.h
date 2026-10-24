/* macOS stub SDK (runtime/stub-sdk/macos/README.md): compile checks only. */
#ifndef ZELL_STUB_SETJMP_H
#define ZELL_STUB_SETJMP_H
typedef int jmp_buf[48];
typedef int sigjmp_buf[49];
int setjmp(jmp_buf);
void longjmp(jmp_buf, int) __attribute__((noreturn));
int _setjmp(jmp_buf);
void _longjmp(jmp_buf, int) __attribute__((noreturn));
int sigsetjmp(sigjmp_buf, int);
void siglongjmp(sigjmp_buf, int) __attribute__((noreturn));
#endif
