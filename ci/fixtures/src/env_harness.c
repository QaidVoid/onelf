/* The environment-clearing re-exec fixture: the first launch wipes the
 * process environment with clearenv() and execs /proc/self/exe, the
 * documented sandbox simulation. The re-executed process must find the
 * packaged [env] records applied again by the bundled constructor and
 * report PASS on stdout. */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (getenv("ONELF_IT_RX")) {
        const char *v = getenv("ONELF_IT_VAR");
        int ok = v && !strcmp(v, "survived");
        printf("ENV_%s v=[%s]\n", ok ? "PASS" : "FAIL", v ? v : "(null)");
        fflush(stdout);
        return ok ? 0 : 1;
    }
    clearenv();
    setenv("ONELF_IT_RX", "1", 1);
    execv("/proc/self/exe", argv);
    return 3;
}
