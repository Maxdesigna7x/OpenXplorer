#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Sign a CI-built bundle and stage the matching GitHub Pages repository.
set -eu

bundle=${1:?input bundle}
site=${2:?Pages directory}
fingerprint=${3:?signing key fingerprint}
app_id=io.winspace.Development.Native
url=https://maxdesigna7x.github.io/OpenXplorer/flatpak
runtime=https://dl.flathub.org/repo/flathub.flatpakrepo

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
gpg --batch --export "$fingerprint" > "$work/key.gpg"
test -s "$work/key.gpg"
mkdir -p "$site/flatpak"
repo="$site/flatpak"
ostree --repo="$repo" init --mode=archive
flatpak build-import-bundle --gpg-sign="$fingerprint" --no-update-summary "$repo" "$bundle"
test "$(ostree --repo="$repo" refs)" = "app/$app_id/x86_64/stable"
flatpak build-update-repo --title="OpenXplorer RANDY" --default-branch=stable \
    --gpg-sign="$fingerprint" --gpg-import="$work/key.gpg" "$repo"
flatpak build-bundle --repo-url="$url" --runtime-repo="$runtime" --gpg-keys="$work/key.gpg" \
    "$repo" "$work/$app_id.flatpak" "$app_id" stable
mv "$work/$app_id.flatpak" "$bundle"

key=$(base64 -w0 "$work/key.gpg")
cat > "$site/OpenXplorer.flatpakrepo" <<REPO
[Flatpak Repo]
Title=OpenXplorer RANDY
Url=$url
DefaultBranch=stable
GPGKey=$key
REPO

cat > "$site/OpenXplorer.flatpakref" <<REF
[Flatpak Ref]
Title=OpenXplorer RANDY
Name=$app_id
Branch=stable
Url=$url
SuggestRemoteName=openxplorer-randy
IsRuntime=false
RuntimeRepo=$runtime
GPGKey=$key
REF
