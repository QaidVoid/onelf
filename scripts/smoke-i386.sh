#!/usr/bin/env bash
# Smoke-test a native i686 onelf package on a real 32-bit userspace.
#
# Builds onelf for i686-unknown-linux-musl, packs a hello-world into a
# fully-native i686 package, and runs it inside an i386 container (via podman).
# On x86_64 hosts the i386 container runs natively through the kernel's IA32
# support, so no qemu is involved.
#
# Prerequisites (bootlin toolchains under /opt/bootlin, the rust musl target,
# and podman) are probed up front; when any is missing the script prints SKIP
# and exits 0 so it is safe to wire into CI matrices that lack them.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${ONELF_SMOKE_IMAGE:-docker.io/i386/debian:bookworm}"
GLIBC=/opt/bootlin/x86-i686-glibc
MUSL=/opt/bootlin/x86-i686-musl
I686_GCC="$GLIBC/bin/i686-linux-gcc"
MUSL_GCC="$MUSL/bin/i686-linux-gcc"
SYSLIB="$GLIBC/i686-buildroot-linux-gnu/sysroot/lib"

skip() { echo "SKIP: $*"; exit 0; }
fail() { echo "FAIL: $*"; exit 1; }

command -v podman >/dev/null 2>&1 || skip "podman not found"
[ -x "$I686_GCC" ] || skip "i686 glibc gcc not found ($I686_GCC)"
[ -x "$MUSL_GCC" ] || skip "i686 musl gcc not found ($MUSL_GCC)"
rustup target list --installed 2>/dev/null | grep -qx i686-unknown-linux-musl \
    || skip "rust target i686-unknown-linux-musl not installed"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# A parent `cargo test`/`cargo build` exports this crate's build-script
# rustc-env vars (ONELF_RT_PATH, ONELF_BOOTSTRAP_*, ONELF_ENV_*, ...) into our
# environment. Left set, the nested build below would take the ONELF_RT_PATH
# escape hatch and embed the parent's (x86_64) runtime instead of an i686 one.
unset ONELF_RT_PATH ONELF_RT_UPDATE_PATH ONELF_PAYLOAD_DIR ONELF_MUSL_CC
for a in X86_64 AARCH64 I686; do
    unset "ONELF_BOOTSTRAP_$a" "ONELF_ENV_$a"
done

echo ">> building native i686 onelf (this compiles the i686 runtime + payloads)"
CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_LINKER="$MUSL_GCC" \
    cargo build --release --manifest-path "$REPO/Cargo.toml" -p onelf \
    --target i686-unknown-linux-musl --target-dir "$WORK/build" >/dev/null
ONELF="$WORK/build/i686-unknown-linux-musl/release/onelf"

echo ">> building an i686 hello-world"
printf '#include <stdio.h>\nint main(){puts("ONELF_I386_SMOKE_OK");return 0;}\n' >"$WORK/app.c"
"$I686_GCC" -O2 -o "$WORK/app" "$WORK/app.c"

echo ">> bundling + packing into a native i686 package"
mkdir -p "$WORK/appdir"
LD_LIBRARY_PATH="$SYSLIB" "$ONELF" bundle-libs "$WORK/appdir" --from-binary "$WORK/app" >/dev/null
"$ONELF" pack --command bin/app --output "$WORK/app.onelf" "$WORK/appdir" >/dev/null

# The package's outer executable must itself be a 32-bit i386 ELF.
hdr="$(readelf -hW "$WORK/app.onelf" 2>/dev/null || true)"
[[ "$hdr" == *"Intel 80386"* ]] || fail "packed executable is not a 32-bit i386 ELF"

echo ">> running the native i686 package in $IMAGE"
dev=()
[ -e /dev/fuse ] && dev=(--device /dev/fuse --cap-add SYS_ADMIN)
out="$(podman run --rm --platform linux/386 "${dev[@]}" \
    -v "$WORK":/work:ro -w /work -e HOME=/tmp "$IMAGE" /work/app.onelf 2>&1 || true)"
echo "$out" | sed 's/^/   /'

if [[ "$out" == *ONELF_I386_SMOKE_OK* ]]; then
    echo "PASS: native i686 package ran on $IMAGE"
    exit 0
fi
fail "expected ONELF_I386_SMOKE_OK in output"
