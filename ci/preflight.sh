#!/bin/sh
# Capability preflight for the CI jobs that run capability-dependent
# pipeline tests. Each job claims a capability set in
# ONELF_REQUIRED_CAPS; this script checks exactly that set before cargo
# runs, so a runner missing a claimed capability fails here with a named
# message instead of green on skipped tests, and a runner that still has
# a capability the job forbids fails here instead of covering a claim
# that is not true.
#
# Usage: preflight.sh JOB, where JOB is one of:
#   namespace-fuse    needs unprivileged user namespaces that can mount
#                     a tmpfs, fusermount3, /dev/fuse, and real FUSE
#                     mounts both through the helper and inside a user
#                     namespace
#   helper-fuse       needs fusermount3, /dev/fuse and a real FUSE mount
#                     through the helper, and needs unprivileged user
#                     namespaces to be denied
#   no-fuse           needs fusermount3, setuid fusermount and /dev/fuse
#                     all absent, so extraction is the only launch path
#   cache             needs a C compiler
#   privileged-import needs uid 0, fusermount3, /dev/fuse and a real FUSE
#                     mount as root
#   media             needs a C compiler, strace, the GStreamer tools and
#                     scanner, ALSA headers and configuration, and a
#                     PipeWire server answering pw-cli
#   distro            needs docker with a daemon, and a real run with a
#                     bind mount, for the pinned distro fixture tests
#
# The FUSE probes mount a one-file squashfs image with squashfuse, so
# the jobs that claim FUSE install squashfuse and squashfs-tools.

set -u

job="${1:-}"
[ -n "$job" ] || {
    echo "usage: preflight.sh namespace-fuse|helper-fuse|no-fuse|cache|privileged-import|media|distro" >&2
    exit 2
}

die() {
    echo "preflight [$job]: $1" >&2
    exit 1
}

has_helper() {
    command -v fusermount3 >/dev/null 2>&1
}

# A setuid fusermount is the runtime's own fallback helper; one without
# the bit cannot mount for an ordinary user.
has_legacy_helper() {
    legacy=$(command -v fusermount 2>/dev/null) || return 1
    [ -u "$legacy" ]
}

has_fuse_dev() {
    [ -e /dev/fuse ]
}

# A real unprivileged user namespace: the caller maps itself to root in
# a fresh mount namespace and mounts a tmpfs there, which is what the
# namespace launch modes do.
userns_works() {
    unshare --user --map-root-user --mount -- \
        sh -c 'mount -t tmpfs onelf-probe /mnt' 2>/dev/null
}

# A real FUSE mount: serve a one-file squashfs image with squashfuse,
# read the file back through the mount, and unmount. With "userns" the
# mount happens inside an unprivileged user namespace, where libfuse
# mounts directly; otherwise an ordinary user mounts through the
# fusermount3 helper, and root mounts directly.
fuse_mount_works() {
    command -v squashfuse >/dev/null 2>&1 || return 1
    command -v mksquashfs >/dev/null 2>&1 || return 1
    probe=$(mktemp -d) || return 1
    mkdir "$probe/src" "$probe/mnt"
    echo ok > "$probe/src/probe"
    mksquashfs "$probe/src" "$probe/img" -quiet -no-progress >/dev/null 2>&1 || {
        rm -rf "$probe"
        return 1
    }
    if [ "${1:-}" = userns ]; then
        # shellcheck disable=SC2016 # expanded by the inner shell
        unshare --user --map-root-user --mount -- sh -c '
            squashfuse "$1/img" "$1/mnt" || exit 1
            [ "$(cat "$1/mnt/probe")" = ok ]
            status=$?
            umount "$1/mnt"
            exit "$status"
        ' sh "$probe" 2>/dev/null
        status=$?
    else
        status=1
        if squashfuse "$probe/img" "$probe/mnt" 2>/dev/null; then
            [ "$(cat "$probe/mnt/probe" 2>/dev/null)" = ok ] && status=0
            fusermount3 -u "$probe/mnt" 2>/dev/null || umount "$probe/mnt" 2>/dev/null
        fi
    fi
    rm -rf "$probe"
    return "$status"
}

has_cc() {
    command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1
}

# The scanner under any of the layouts distributions install it in,
# Debian's multiarch gstreamer1.0 directory included.
has_gst_scanner() {
    for scanner in /usr/libexec/gstreamer-1.0/gst-plugin-scanner \
        /usr/lib*/gstreamer-1.0/gst-plugin-scanner \
        /usr/lib/*/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner; do
        [ -x "$scanner" ] && return 0
    done
    return 1
}

has() {
    command -v "$1" >/dev/null 2>&1
}

case "$job" in
distro)
    # The probe image is the Alpine fixture's own pinned base.
    image=$(sed -n 's/^FROM //p' "$(dirname "$0")/fixtures/Dockerfile.alpine-3.20")
    [ -n "$image" ] || die "cannot read the pinned base image from ci/fixtures/Dockerfile.alpine-3.20"
    command -v docker >/dev/null 2>&1 || die "needs docker on PATH; install docker"
    docker info >/dev/null 2>&1 || die "needs a reachable docker daemon; docker info failed"
    probe=$(mktemp -d) || die "needs a writable temp directory for the bind mount probe"
    docker run --rm --network none -v "$probe":/probe "$image" /bin/sh -c 'echo ok > /probe/probe' >/dev/null 2>&1 \
        || die "needs docker run with a bind mount; the probe container failed"
    [ -f "$probe/probe" ] || die "the docker bind mount did not reach the host"
    rm -rf "$probe"
    ;;
namespace-fuse)
    userns_works || die "needs unprivileged user namespaces; unshare --user --map-root-user --mount failed. Set kernel.apparmor_restrict_unprivileged_userns=0"
    has_helper || die "needs fusermount3 on PATH; install fuse3"
    has_fuse_dev || die "needs /dev/fuse; run modprobe fuse"
    fuse_mount_works || die "needs a real FUSE mount through fusermount3; the squashfuse probe failed"
    fuse_mount_works userns || die "needs a real FUSE mount inside a user namespace; the squashfuse probe failed"
    ;;
helper-fuse)
    has_helper || die "needs fusermount3 on PATH; install fuse3"
    has_fuse_dev || die "needs /dev/fuse; run modprobe fuse"
    fuse_mount_works || die "needs a real FUSE mount through fusermount3; the squashfuse probe failed"
    if userns_works; then
        die "must NOT have unprivileged user namespaces; set kernel.apparmor_restrict_unprivileged_userns=1 so the helper path is the only mount path"
    fi
    ;;
no-fuse)
    if has_helper; then
        die "must NOT have fusermount3; with it the extraction fallback is not the path under test"
    fi
    if has_legacy_helper; then
        die "must NOT have a setuid fusermount; with it the extraction fallback is not the path under test"
    fi
    if has_fuse_dev; then
        die "must NOT have /dev/fuse; with it a mount can succeed and the fallback claim is false"
    fi
    ;;
cache)
    has_cc || die "needs a C compiler (cc or gcc)"
    ;;
privileged-import)
    [ "$(id -u)" = "0" ] || die "needs uid 0; run this job's steps under sudo or a privileged container"
    has_helper || die "needs fusermount3 on PATH; install fuse3"
    has_fuse_dev || die "needs /dev/fuse; run modprobe fuse"
    fuse_mount_works || die "needs a real FUSE mount as root; the squashfuse probe failed"
    ;;
media)
    has_cc || die "needs a C compiler (cc or gcc)"
    has strace || die "needs strace on PATH; install strace"
    has musl-gcc || die "needs musl-gcc on PATH; install musl-tools"
    [ -e /lib/ld-musl-x86_64.so.1 ] || die "needs the musl loader at /lib/ld-musl-x86_64.so.1; install musl"
    has gst-launch-1.0 || die "needs gst-launch-1.0 on PATH; install gstreamer1.0-tools"
    has_gst_scanner || die "needs gst-plugin-scanner; install libgstreamer1.0-0"
    gst-inspect-1.0 audiotestsrc >/dev/null 2>&1 || die "needs the audiotestsrc element; install gstreamer1.0-plugins-base"
    gst-inspect-1.0 filesink >/dev/null 2>&1 || die "needs the filesink element; install libgstreamer1.0-0"
    pkg-config --exists alsa || die "needs ALSA headers; install libasound2-dev"
    [ -f /usr/share/alsa/alsa.conf ] || die "needs /usr/share/alsa/alsa.conf; install libasound2-data"
    [ -f /usr/share/pipewire/client.conf ] || die "needs /usr/share/pipewire/client.conf; install pipewire"
    pw-cli info 0 >/dev/null 2>&1 || die "needs a PipeWire server answering pw-cli info 0; start pipewire with XDG_RUNTIME_DIR=/run/user/\$(id -u)"
    ;;
*)
    echo "preflight: unknown job '$job'" >&2
    exit 2
    ;;
esac

echo "preflight [$job]: ok"
