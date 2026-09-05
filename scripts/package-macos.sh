#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_dir="${CAMRTSP_DIST_DIR:-$project_root/dist}"
app="$output_dir/camrtsp.app"
identity="${CAMRTSP_CODESIGN_IDENTITY:--}"

mkdir -p "$app/Contents/MacOS"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target add "$target"
  cargo build --locked --manifest-path "$project_root/Cargo.toml" --release --target "$target" -p camrtsp
done
lipo -create \
  "$project_root/target/aarch64-apple-darwin/release/camrtsp" \
  "$project_root/target/x86_64-apple-darwin/release/camrtsp" \
  -output "$app/Contents/MacOS/camrtsp"
cp "$project_root/packaging/macos/Info.plist" "$app/Contents/Info.plist"
codesign --force --options runtime --entitlements "$project_root/packaging/macos/camrtsp.entitlements" --sign "$identity" "$app"
ln -sfn "$app/Contents/MacOS/camrtsp" "$output_dir/camrtsp"
echo "Created $app and $output_dir/camrtsp"

archive="$output_dir/camrtsp-macos-universal.tar.gz"
cp "$project_root/README.md" "$project_root/LICENSE-MIT" "$project_root/LICENSE-APACHE" "$output_dir/"
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$output_dir" camrtsp.app camrtsp README.md LICENSE-MIT LICENSE-APACHE
(cd "$output_dir" && shasum -a 256 "$(basename "$archive")" > "$(basename "$archive").sha256")
echo "Created $archive"
