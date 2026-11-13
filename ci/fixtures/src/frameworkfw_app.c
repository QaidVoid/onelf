/* The descriptor-free framework fixture's program: enumerates the
 * plugin directory through its compiled-in spelling, which the build
 * rewrites to the anchor, loads its plugin through that spelling, and
 * reads the anchored data directory and the paired configuration path.
 * What each read answers tells the fixture whether the launch bound the
 * pair or left it in the host's namespace, and the MAP line shows where
 * the plugin was mapped from. */
#include <dirent.h>
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

#ifndef FW_PLUGIN_DIR
#define FW_PLUGIN_DIR "unset"
#endif
#ifndef FW_CONF_PATH
#define FW_CONF_PATH "unset"
#endif
#ifndef FW_CFG_PATH
#define FW_CFG_PATH "unset"
#endif

int main(void) {
    char line[4096];
    printf("dir=%s\n", FW_PLUGIN_DIR);
    snprintf(line, sizeof line, "%s/libfwgood.so", FW_PLUGIN_DIR);
    printf("loaded=%s\n", dlopen(line, RTLD_NOW) ? "yes" : "no");
    FILE *maps = fopen("/proc/self/maps", "r");
    while (maps && fgets(line, sizeof line, maps)) {
        char *path = strchr(line, '/');
        if (path && strstr(path, "libfwgood")) {
            printf("MAP %s", path);
            break;
        }
    }
    if (maps)
        fclose(maps);
    DIR *d = opendir(FW_PLUGIN_DIR);
    if (!d) {
        printf("dir=missing\n");
    } else {
        struct dirent *e;
        while ((e = readdir(d)))
            if (strstr(e->d_name, ".so"))
                printf("member=%s\n", e->d_name);
        closedir(d);
    }
    FILE *f = fopen(FW_CONF_PATH, "r");
    if (f) {
        char buf[16] = {0};
        size_t n = fread(buf, 1, sizeof buf - 1, f);
        fclose(f);
        printf("conf=%.*s", (int)n, buf);
    } else {
        printf("conf=missing\n");
    }
    f = fopen(FW_CFG_PATH, "r");
    if (f) {
        char buf[16] = {0};
        size_t n = fread(buf, 1, sizeof buf - 1, f);
        fclose(f);
        printf("cfg=%.*s", (int)n, buf);
    } else {
        printf("cfg=missing\n");
    }
    return 0;
}
