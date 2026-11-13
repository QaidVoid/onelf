#!/bin/sh
# Compile the fixture applications inside a pinned distro image.
#
# Runs in the container with the repository read-only at /repo and the
# scratch tree at /work, using only the toolchain and libraries the
# image installs. Takes the distro tag used as the compiled-in marker
# and the names of the applications to build; with no names it builds
# cli gtk qt script libc env.
#
#   sh /repo/ci/fixtures/assemble.sh ubuntu-24.04 cli gtk qt script libc env audio frameworkfw
set -eu

TAG="${1:?usage: assemble.sh TAG [APP ...]}"
shift
if [ "$#" -gt 0 ]; then
    APPS="$*"
else
    APPS="cli gtk qt script libc env"
fi

SRC=/repo/ci/fixtures/src
APPS_DIR=/work/apps
mkdir -p "$APPS_DIR"

build_cli() {
    mkdir -p "$APPS_DIR/cli/bin"
    gcc -O0 -DDISTRO_TAG="\"$TAG\"" -o "$APPS_DIR/cli/bin/cli" "$SRC/cli_app.c"
}

build_gtk() {
    mkdir -p "$APPS_DIR/gtk/bin"
    gcc -O0 $(pkg-config --cflags gtk+-3.0) \
        -o "$APPS_DIR/gtk/bin/gtk" "$SRC/gui_app.c" \
        $(pkg-config --libs gtk+-3.0)
}

build_qt() {
    mkdir -p "$APPS_DIR/qt/bin"
    g++ -O0 $(pkg-config --cflags Qt6Core) \
        -o "$APPS_DIR/qt/bin/qt" "$SRC/qt_app.cpp" \
        $(pkg-config --libs Qt6Core)
}

build_script() {
    mkdir -p "$APPS_DIR/script/bin"
    gcc -O0 -o "$APPS_DIR/script/bin/helper" "$SRC/helper_app.c"
    cat > "$APPS_DIR/script/bin/run" <<'RUN'
#!/bin/sh
echo SCRIPT_OK
exec "$(dirname "$0")/helper"
RUN
    chmod +x "$APPS_DIR/script/bin/run"
}

build_libc() {
    mkdir -p "$APPS_DIR/libc/bin"
    gcc -O0 -o "$APPS_DIR/libc/bin/libc" "$SRC/libc_app.c"
}

build_env() {
    mkdir -p "$APPS_DIR/env/bin"
    gcc -O0 -o "$APPS_DIR/env/bin/harness" "$SRC/env_harness.c"
}

build_audio() {
    mkdir -p "$APPS_DIR/audio/bin"
    gcc -O0 -DDISTRO_TAG="\"$TAG\"" $(pkg-config --cflags alsa) \
        -o "$APPS_DIR/audio/bin/audio" "$SRC/audio_app.c" \
        $(pkg-config --libs alsa)
}

build_frameworkfw() {
    mkdir -p /work/fwroot/usr/lib/fwplugins \
        /work/fwroot/usr/share/fwconf \
        "$APPS_DIR/frameworkfw/bin" \
        "$APPS_DIR/frameworkfw/share/fwcfg" \
        "$APPS_DIR/hostcfg"
    echo CONF > /work/fwroot/usr/share/fwconf/a.conf
    echo BUNDLED > "$APPS_DIR/frameworkfw/share/fwcfg/set.conf"
    echo HOST > "$APPS_DIR/hostcfg/set.conf"
    echo bundled > /work/fwroot/usr/lib/fwplugins/readme.txt
    gcc -O0 -o /work/fwroot/usr/lib/fwplugins/libfwgood.so \
        -shared -fPIC "$SRC/helper_app.c"
    gcc -O0 \
        -DFW_PLUGIN_DIR="\"/work/fwroot/usr/lib/fwplugins\"" \
        -DFW_CONF_DIR="\"/work/fwroot/usr/share/fwconf\"" \
        -DFW_CFG_PATH="\"/work/apps/hostcfg\"" \
        -shared -fPIC -Wl,-soname,libfwfw.so.1 \
        -o /work/fwroot/usr/lib/libfwfw.so.1 "$SRC/frameworkfw_lib.c"
    gcc -O0 \
        -DFW_PLUGIN_DIR="\"/work/fwroot/usr/lib/fwplugins\"" \
        -DFW_CONF_PATH="\"/work/fwroot/usr/share/fwconf/a.conf\"" \
        -DFW_CFG_PATH="\"/work/apps/hostcfg/set.conf\"" \
        -o "$APPS_DIR/frameworkfw/bin/fwfw" "$SRC/frameworkfw_app.c" \
        -L/work/fwroot/usr/lib -Wl,--no-as-needed -l:libfwfw.so.1 -ldl
}

for app in $APPS; do
    "build_$app"
done

echo "BUILD_OK $TAG"
