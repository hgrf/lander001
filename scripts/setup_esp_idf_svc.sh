#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="$repo_root/vendor/esp-idf-svc"
patch_file="$repo_root/patches/esp-idf-svc.patch"
upstream_repo="https://github.com/esp-rs/esp-idf-svc.git"
upstream_ref="372dd81884ce3537ef5087eba3e0483a83c83057"

mkdir -p "$repo_root/vendor"

if [[ -d "$target_dir/.git" ]]; then
  echo "[esp-idf-svc] Reusing existing clone in $target_dir"
  git -C "$target_dir" fetch origin
else
  rm -rf "$target_dir"
  echo "[esp-idf-svc] Cloning $upstream_repo"
  git clone "$upstream_repo" "$target_dir"
fi

git -C "$target_dir" checkout --detach "$upstream_ref"
git -C "$target_dir" reset --hard "$upstream_ref"

if git -C "$target_dir" apply --reverse --check "$patch_file" >/dev/null 2>&1; then
  echo "[esp-idf-svc] Patch already applied"
else
  echo "[esp-idf-svc] Applying disconnect reason patch"
  git -C "$target_dir" apply "$patch_file"
fi

echo "[esp-idf-svc] Ready at $target_dir"
