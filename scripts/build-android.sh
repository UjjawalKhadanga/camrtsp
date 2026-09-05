#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
android_sdk="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
android_ndk="${ANDROID_NDK_HOME:-$android_sdk/ndk/27.1.12297006}"
target_dir="$project_root/apps/android/app/src/main/jniLibs"

if ! command -v cargo-ndk >/dev/null 2>&1; then
  echo "Install cargo-ndk first: cargo install cargo-ndk" >&2
  exit 1
fi

mkdir -p "$target_dir"
ANDROID_NDK_HOME="$android_ndk" cargo ndk --manifest-path "$project_root/Cargo.toml" --platform 26 --target arm64-v8a --target x86_64 --output-dir "$target_dir" build --locked --release -p camrtsp-android
