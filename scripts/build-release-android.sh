#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
signing_env="$repo_root/.signing/release.env"
builder=${WAKER_CARGO_APK2_NATIVE:-"$HOME/sysadmin/ghostbsd/tools/android-native-build/cargo-apk2-native"}

if [ ! -r "$signing_env" ]; then
    echo "Missing private signing environment: $signing_env" >&2
    exit 1
fi
if [ ! -x "$builder" ]; then
    echo "Android build wrapper not executable: $builder" >&2
    exit 1
fi

# shellcheck disable=SC1090
. "$signing_env"

: "${CARGO_APK_RELEASE_KEYSTORE:?missing CARGO_APK_RELEASE_KEYSTORE}"
: "${CARGO_APK_RELEASE_KEYSTORE_PASSWORD:?missing CARGO_APK_RELEASE_KEYSTORE_PASSWORD}"

cd "$repo_root"
exec "$builder" build --release -p waker-app --lib --target aarch64-linux-android
