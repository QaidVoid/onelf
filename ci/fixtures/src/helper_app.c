/* The helper binary of the script fixture: the shell script entrypoint
 * execs this bundled ELF, so the run proves both the script interpreter
 * path and a bundled companion binary resolve inside the package. */
#include <stdio.h>

int main(void) {
    printf("HELPER_OK\n");
    return 0;
}
