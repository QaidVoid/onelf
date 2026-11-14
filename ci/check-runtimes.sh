#!/bin/sh
# Inspect the slim and update runtimes that crates/onelf/build.rs built
# for one cargo profile: each must be a static executable (no PT_INTERP,
# no DT_NEEDED) and carry the onelf capability note with exactly the
# features that variant advertises.
#
# Usage: check-runtimes.sh [PROFILE], PROFILE defaulting to release.
# Reads CARGO_TARGET_DIR, defaulting to ./target.

set -eu

profile="${1:-release}"
target_dir="${CARGO_TARGET_DIR:-target}"

fail() {
    echo "check-runtimes: $1" >&2
    exit 1
}

# The note description is the magic OCAP, the metadata version 1 and the
# capability bits: selection-view and anchors for slim, plus store-import
# for update.
for spec in slim:05 update:07; do
    variant="${spec%%:*}"
    bits="${spec#*:}"
    # The newest build directory, since a restored cache may hold stale
    # ones beside it. Cargo names it build/onelf-<hash>, or build/onelf/<hash>
    # from 1.100 on.
    tail="out/onelf-rt-$variant/x86_64-unknown-linux-musl/$profile/onelf-rt"
    # shellcheck disable=SC2012 # the paths are cargo's own, no odd names
    rt=$(ls -t "$target_dir/$profile"/build/onelf-*/"$tail" "$target_dir/$profile"/build/onelf/*/"$tail" 2>/dev/null | head -n 1)
    [ -n "$rt" ] || fail "no $profile $variant runtime under $target_dir; build onelf first"

    phdrs=$(readelf -lW "$rt") || fail "readelf -l failed on $rt"
    echo "$phdrs" | grep -q '^Program Headers:' || fail "readelf printed no program headers for $rt"
    if echo "$phdrs" | grep -q 'INTERP'; then
        fail "$variant runtime has a PT_INTERP segment: $rt"
    fi

    dynamic=$(readelf -dW "$rt") || fail "readelf -d failed on $rt"
    if echo "$dynamic" | grep -q '(NEEDED)'; then
        fail "$variant runtime has DT_NEEDED entries: $rt"
    fi

    notes=$(readelf -nW "$rt") || fail "readelf -n failed on $rt"
    echo "$notes" | grep -Eq "ONELF +0x0000000c.*4f 43 41 50 01 00 00 00 $bits 00 00 00" \
        || fail "$variant runtime lacks the capability note with bits $bits: $rt
$notes"

    echo "check-runtimes: $profile $variant runtime is static and advertises bits $bits"
done
