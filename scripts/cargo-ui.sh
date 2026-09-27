#!/bin/sh
set -eu

# GPUI's dispatch bindgen needs the active macOS SDK when libclang is external.
if [ "$(uname -s)" = Darwin ]; then
    sideport_sdk=$(xcrun --sdk macosx --show-sdk-path)
    BINDGEN_EXTRA_CLANG_ARGS="${BINDGEN_EXTRA_CLANG_ARGS:-} -isysroot \"$sideport_sdk\""
    export BINDGEN_EXTRA_CLANG_ARGS
fi

exec cargo "$@"
