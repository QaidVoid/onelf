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
#   namespace-fuse    needs unprivileged user namespaces, fusermount3
#                     and /dev/fuse
#   helper-fuse       needs fusermount3 and /dev/fuse, and needs
#                     unprivileged user namespaces to be denied
#   no-fuse           needs fusermount3, setuid fusermount and /dev/fuse
#                     all absent, so extraction is the only launch path
#   cache             needs a C compiler
#   privileged-import needs uid 0, fusermount3 and /dev/fuse

set -u

job="${1:-}"
[ -n "$job" ] || {
    echo "usage: preflight.sh namespace-fuse|helper-fuse|no-fuse|cache|privileged-import" >&2
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

# The probe the tests themselves use: an unprivileged user namespace
# with a mount namespace the caller can map itself to root in.
userns_works() {
    unshare --user --map-root-user --mount -- true 2>/dev/null
}

has_cc() {
    command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1
}

case "$job" in
namespace-fuse)
    userns_works || die "needs unprivileged user namespaces; unshare --user --map-root-user --mount failed. Set kernel.apparmor_restrict_unprivileged_userns=0"
    has_helper || die "needs fusermount3 on PATH; install fuse3"
    has_fuse_dev || die "needs /dev/fuse; run modprobe fuse"
    ;;
helper-fuse)
    has_helper || die "needs fusermount3 on PATH; install fuse3"
    has_fuse_dev || die "needs /dev/fuse; run modprobe fuse"
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
    ;;
*)
    echo "preflight: unknown job '$job'" >&2
    exit 2
    ;;
esac

echo "preflight [$job]: ok"
