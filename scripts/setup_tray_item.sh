#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="$repo_root/vendor/tray-item"
patch_file="$repo_root/patches/tray-item-macos.patch"
upstream_repo="https://github.com/olback/tray-item-rs.git"
upstream_ref="07b6e4802e0536e830be8f021d76989105849174"

mkdir -p "$repo_root/vendor"

if [[ -d "$target_dir/.git" ]]; then
  echo "[tray-item] Reusing existing clone in $target_dir"
  git -C "$target_dir" fetch origin
else
  rm -rf "$target_dir"
  echo "[tray-item] Cloning $upstream_repo"
  git clone "$upstream_repo" "$target_dir"
fi

git -C "$target_dir" checkout --detach "$upstream_ref"

if git -C "$target_dir" apply --reverse --check "$patch_file" >/dev/null 2>&1; then
  echo "[tray-item] Patch already applied"
else
  echo "[tray-item] Applying macOS tray patch"
  git -C "$target_dir" apply "$patch_file"
fi

echo "[tray-item] Ready at $target_dir"
