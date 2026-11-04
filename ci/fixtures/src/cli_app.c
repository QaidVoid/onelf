/* The CLI fixture: prints a marker carrying the distro tag it was
 * compiled with, so a run of the packed package on any image names both
 * where it was built and proves where it is running. */
#include <stdio.h>

#ifndef DISTRO_TAG
#define DISTRO_TAG "unknown"
#endif

int main(void) {
    printf("CLI_OK %s\n", DISTRO_TAG);
    return 0;
}
