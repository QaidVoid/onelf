/* The libc family fixture: prints which C library it was built against
 * and, under glibc, its runtime version. A package of the glibc build
 * that prints `libc: glibc` while running on Alpine proves the bundled
 * glibc answered and the host's musl was never consulted. */
#include <stdio.h>

#ifdef __GLIBC__
#include <gnu/libc-version.h>
int main(void) {
    printf("libc: glibc %s\n", gnu_get_libc_version());
    return 0;
}
#else
int main(void) {
    printf("libc: musl\n");
    return 0;
}
#endif
