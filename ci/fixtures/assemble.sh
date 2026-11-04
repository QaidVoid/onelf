#!/bin/sh
# Compile the fixture applications inside a pinned distro image.
#
# Runs in the container with the repository read-only at /repo and the
# scratch tree at /work, using only the toolchain and libraries the
# image installs. Takes the distro tag used as the compiled-in marker
# and the names of the applications to build; with no names it builds
# every fixture application under /work/apps.
#
#   sh /repo/ci/fixtures/assemble.sh ubuntu-24.04 cli gtk qt script libc env
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

for app in $APPS; do
    "build_$app"
done

echo "BUILD_OK $TAG"
