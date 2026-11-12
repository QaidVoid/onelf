/* The descriptor-free framework fixture's library: compiles in its
 * plugin directory, its data directory and a configuration path outside
 * its own prefix, so only a recipe pair can answer the third one. The
 * paths come from the assembler, which names the assembled tree. */
#ifndef FW_PLUGIN_DIR
#define FW_PLUGIN_DIR "unset"
#endif
#ifndef FW_CONF_DIR
#define FW_CONF_DIR "unset"
#endif
#ifndef FW_CFG_PATH
#define FW_CFG_PATH "unset"
#endif

const char *fw_dirs[] = {FW_PLUGIN_DIR, FW_CONF_DIR};
const char *fw_cfg = FW_CFG_PATH;

int framework(void) {
    return 1;
}
