#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
signing_env="$repo_root/.signing/release.env"
builder=${WAKER_CARGO_APK2_NATIVE:-"$HOME/sysadmin/ghostbsd/tools/android-native-build/cargo-apk2-native"}
sdk=${ANDROID_HOME:-"$HOME/Android/Sdk"}
build_tools_version=36.0.0
native_build_tools="$sdk/build-tools/$build_tools_version"
game_build_tools_url=https://dl.google.com/android/repository/build-tools_r36_linux.zip
game_build_tools_sha256=5d9ac77fb6ff43d9da518a337b4fcf8f9097113df531d99ccefe80ef7ce8250b
game_sdk="$repo_root/target/android-game-sdk"
game_build_tools_archive="$repo_root/target/android-game-build-tools.zip"
game_build_tools_stamp="$game_sdk/.build-tools-sha256"

if [ ! -r "$signing_env" ]; then
    echo "Missing private signing environment: $signing_env" >&2
    exit 1
fi
if [ ! -x "$builder" ]; then
    echo "Android build wrapper not executable: $builder" >&2
    exit 1
fi
for tool in aapt2 zipalign apksigner; do
    if [ ! -x "$native_build_tools/$tool" ]; then
        echo "Missing native Android build tool: $native_build_tools/$tool" >&2
        exit 1
    fi
done
for tool in fetch gradle sha256 unzip; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "Required Android build tool not found in PATH: $tool" >&2
        exit 1
    fi
done

# shellcheck disable=SC1090
. "$signing_env"

: "${CARGO_APK_RELEASE_KEYSTORE:?missing CARGO_APK_RELEASE_KEYSTORE}"
: "${CARGO_APK_RELEASE_KEYSTORE_PASSWORD:?missing CARGO_APK_RELEASE_KEYSTORE_PASSWORD}"

cd "$repo_root"

# cargo-apk2 remains the native Rust build driver. Its APK is intermediate:
# the final package is assembled by Gradle so AndroidX GameActivity and
# GameTextInput dependencies are resolved correctly.
"$builder" build --release -p waker-app --lib --target aarch64-linux-android

intermediate_apk="$repo_root/target/release/apk/waker_app.apk"
native_library="$repo_root/target/aarch64-linux-android/release/libwaker_app.so"
if [ ! -r "$intermediate_apk" ] || [ ! -r "$native_library" ]; then
    echo "Native Android build did not produce the expected outputs" >&2
    exit 1
fi

package_line=$("$native_build_tools/aapt2" dump badging "$intermediate_apk" | sed -n '1p')
version_code=$(printf '%s\n' "$package_line" | sed -n "s/.*versionCode='\([^']*\)'.*/\1/p")
version_name=$(printf '%s\n' "$package_line" | sed -n "s/.*versionName='\([^']*\)'.*/\1/p")
if [ -z "$version_code" ] || [ -z "$version_name" ]; then
    echo "Could not derive Android version from cargo-apk2 output" >&2
    exit 1
fi

# AGP expects a complete official Build Tools package. Manta's normal Android
# SDK uses native FreeBSD build-tool binaries, so keep an official Linux Build
# Tools copy only inside target/ for AGP's Java-side tooling and metadata.
# aapt2 is explicitly overridden below with the working FreeBSD binary.
if [ ! -r "$game_build_tools_stamp" ] ||
   [ "$(cat "$game_build_tools_stamp" 2>/dev/null || true)" != "$game_build_tools_sha256" ]; then
    rm -rf "$game_sdk"
    mkdir -p "$game_sdk/build-tools"

    archive_ok=false
    if [ -r "$game_build_tools_archive" ] &&
       [ "$(sha256 -q "$game_build_tools_archive")" = "$game_build_tools_sha256" ]; then
        archive_ok=true
    fi
    if [ "$archive_ok" != true ]; then
        rm -f "$game_build_tools_archive"
        fetch -qo "$game_build_tools_archive" "$game_build_tools_url"
    fi
    actual_sha256=$(sha256 -q "$game_build_tools_archive")
    if [ "$actual_sha256" != "$game_build_tools_sha256" ]; then
        echo "Android Build Tools archive checksum mismatch" >&2
        exit 1
    fi

    unzip -q "$game_build_tools_archive" -d "$game_sdk/build-tools"
    mv "$game_sdk/build-tools/android-16" "$game_sdk/build-tools/$build_tools_version"
    ln -s "$sdk/platforms" "$game_sdk/platforms"
    printf '%s\n' "$game_build_tools_sha256" > "$game_build_tools_stamp"
elif [ ! -e "$game_sdk/platforms" ]; then
    ln -s "$sdk/platforms" "$game_sdk/platforms"
fi

jni_dir="$repo_root/target/android-jniLibs/arm64-v8a"
mkdir -p "$jni_dir"
cp "$native_library" "$jni_dir/libwaker_app.so"

ANDROID_HOME="$game_sdk" ANDROID_SDK_ROOT="$game_sdk" gradle --no-daemon -p "$repo_root/android-gradle"     -Pandroid.aapt2FromMavenOverride="$native_build_tools/aapt2"     -PwakerVersionName="$version_name"     -PwakerVersionCode="$version_code"     :app:assembleRelease

unsigned_apk="$repo_root/android-gradle/app/build/outputs/apk/release/app-release-unsigned.apk"
aligned_apk="$repo_root/target/android-game-activity-aligned.apk"
final_apk="$repo_root/target/release/apk/waker_app.apk"
if [ ! -r "$unsigned_apk" ]; then
    echo "Gradle did not produce the expected release APK" >&2
    exit 1
fi

"$native_build_tools/zipalign" -f 4 "$unsigned_apk" "$aligned_apk"
"$native_build_tools/apksigner" sign     --ks "$CARGO_APK_RELEASE_KEYSTORE"     --ks-pass env:CARGO_APK_RELEASE_KEYSTORE_PASSWORD     --out "$final_apk"     "$aligned_apk"
"$native_build_tools/apksigner" verify --verbose "$final_apk"

echo "Signed GameActivity APK: $final_apk"