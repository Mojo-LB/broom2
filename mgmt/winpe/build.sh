#!/bin/bash
# winpe/build.sh [out] — build broom-report.exe ("Add hardware", Windows PE side) from broom-report.c
# → out (default ../dist/broom-report.exe). No binary of it is kept in the repository:
# - a release: CI runs this and adds the exe to broom-assets.zip (assets.rs OPTIONAL);
# - a local build: run this, then copy the exe to <Broom's folder on the server>/winpe/broom-report.exe
#   (onboard.rs reporter()); without it Add hardware says so, everything else works.
# Needs: mingw-w64 (Debian/Ubuntu: apt install gcc-mingw-w64-x86-64).
set -euo pipefail
cd "$(dirname "$0")"
out=${1:-../dist/broom-report.exe}
mkdir -p "$(dirname "$out")"
x86_64-w64-mingw32-gcc -O2 -s -static -Wall -Wextra -Werror -o "$out" broom-report.c -lwinhttp
sha256sum "$out"
