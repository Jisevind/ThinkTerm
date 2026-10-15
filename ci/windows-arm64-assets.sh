#!/bin/bash
# Fetches the arm64 builds of the files the Windows package carries beside
# the executables, into assets/windows/arm64 (not tracked). The x86_64 copies
# are checked in under assets/windows; these come from upstream releases
# instead, pinned by version and checksum:
#
#   conhost/  conpty.dll and OpenConsole.exe, from Microsoft's ConPTY package
#   mesa/     opengl32.dll, Mesa's llvmpipe software renderer
#
# There is no arm64 ANGLE. EGL is off by default on Windows, so the arm64
# package goes without libEGL.dll and libGLESv2.dll.
#
# Run from the repository root, on Windows: it needs curl, sha256sum and 7z,
# which the GitHub runner has on PATH.
set -euo pipefail

conpty_version=1.25.260930003
conpty_sha256=02b07b349af66d801159bdf9e440d4a1ce78bb951f37fc8609731665afdae7ee
mesa_version=26.2.5
mesa_sha256=e7df75398d50e07a68bbffa5cff2ece4c85e5f8ef8bda497f560dd04fe60cbbd

dest=assets/windows/arm64
# Relative, so the paths handed to 7z need no translating from Git Bash's
# /tmp to a Windows path.
mkdir -p target
work=$(mktemp -d target/windows-arm64-assets.XXXXXX)
trap 'rm -rf "$work"' EXIT

# url file sha256
fetch() {
  bash ci/retry.sh curl -sSfL -o "$work/$2" "$1"
  echo "$3  $work/$2" | sha256sum -c -
}

fetch "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$conpty_version/microsoft.windows.console.conpty.$conpty_version.nupkg" \
  conpty.nupkg "$conpty_sha256"
fetch "https://github.com/mmozeiko/build-mesa/releases/download/$mesa_version/mesa-llvmpipe-arm64-$mesa_version.7z" \
  mesa.7z "$mesa_sha256"

7z x -y -o"$work/conpty" "$work/conpty.nupkg" >/dev/null
7z x -y -o"$work/mesa" "$work/mesa.7z" opengl32.dll >/dev/null

rm -rf "$dest"
mkdir -p "$dest/conhost" "$dest/mesa"
cp "$work/conpty/runtimes/win-arm64/native/conpty.dll" \
  "$work/conpty/build/native/runtimes/arm64/OpenConsole.exe" \
  "$dest/conhost/"
cp "$work/mesa/opengl32.dll" "$dest/mesa/"
ls -l "$dest"/*
