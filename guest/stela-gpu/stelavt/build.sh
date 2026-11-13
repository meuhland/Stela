#!/bin/bash
# Builds StelaVT.bundle (arm64 + arm64e, x264 linked in) and vttest in ~/vtplugin.
set -e
cd ~/vtplugin
B=StelaVT.bundle/Contents
mkdir -p $B/MacOS
cp Info.plist $B/Info.plist
FW="-framework CoreFoundation -framework CoreMedia -framework CoreVideo -framework VideoToolbox"
slices=()
for arch in arm64 arm64e; do
  X=~/vtplugin/x264-$arch
  if [ ! -f $X/lib/libx264.a ]; then
    echo "no x264 for $arch in $X"; continue
  fi
  clang -arch $arch -O2 -Wall -Wno-deprecated-declarations -bundle -o StelaVT.$arch StelaVT.c \
    -I$X/include $X/lib/libx264.a $FW
  slices+=(StelaVT.$arch)
done
lipo -create "${slices[@]}" -output $B/MacOS/StelaVT
rm -f "${slices[@]}"
codesign -s - -f StelaVT.bundle
clang -arch arm64 -O2 -Wall -Wno-deprecated-declarations -o vttest vttest.c $FW
codesign -s - -f vttest
file $B/MacOS/StelaVT
