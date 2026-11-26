#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
set -euo pipefail
cd "$(dirname "$0")/.."

app_id=io.winspace.Development.Native
bundle=${1:-dist/OpenXplorer-RANDY-2.0.18.flatpak}
program=${2:-native/target/release/openxplorer-native}
if [[ $# -lt 2 ]]; then
    OX_APP_ID=$app_id cargo build --release --locked --manifest-path native/Cargo.toml \
        --package ox-app --bin openxplorer-native
fi

stage=$(mktemp -d)
trap 'rm -r -- "$stage"' EXIT
flatpak build-init "$stage/build" "$app_id" org.gnome.Sdk org.gnome.Platform 51
python3 native/tools/package_data.py --app-id "$app_id" --layout flatpak \
    --program "$program" --destdir "$stage/data"
mv "$stage/data/app/bin" "$stage/data/app/share" "$stage/build/files/"
strip --strip-debug "$stage/build/files/bin/openxplorer-native"
mapfile -t permissions < <(
    sed -n 's/^  - \(--.*\)$/\1/p' native/packaging/flatpak/io.winspace.Development.Native.yml
)
flatpak build-finish --command=openxplorer-native \
    "${permissions[@]}" "$stage/build"

# This repository is temporary and contains only this bundle.
mkdir -p "$stage/repo"/{objects,tmp,state,extensions,refs/{heads,remotes,mirrors}}
printf '[core]\nrepo_version=1\nmode=archive-z2\nmin-free-space-size=500MB\n' > "$stage/repo/config"
flatpak build-export "$stage/repo" "$stage/build" stable
mkdir -p "$(dirname "$bundle")"
flatpak build-bundle --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo \
    "$stage/repo" "$bundle" "$app_id" stable
