#!/bin/bash
# Builds StelaVT.bundle (arm64 + arm64e, x264 linked in) and vttest in
# ~/vtplugin, from StelaVT.c, StelaVD.c, StelaEncoder.h, StelaDecoder.h,
# vttest.c and Info.plist there.
# StelaVT encodes on the host's NVENC through StelaGPU.bundle
# (StelaEncoder.h) and keeps x264 for H.264 when the host has none; StelaVD
# decodes on the host's NVDEC (StelaDecoder.h) and keeps Apple's software
# decoder for every stream the host does not take.
set -e
cd ~/vtplugin
for file in StelaVT.c StelaVD.c StelaEncoder.h StelaDecoder.h vttest.c Info.plist; do
  [ -f $file ] || { echo "~/vtplugin/$file is missing"; exit 1; }
done
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
  clang -arch $arch -O2 -Wall -Wno-deprecated-declarations -bundle -o StelaVT.$arch StelaVT.c StelaVD.c \
    -I. -I$X/include $X/lib/libx264.a $FW
  slices+=(StelaVT.$arch)
done
[ ${#slices[@]} -gt 0 ] || { echo "no slice built"; exit 1; }
lipo -create "${slices[@]}" -output $B/MacOS/StelaVT
rm -f "${slices[@]}"
codesign -s - -f StelaVT.bundle
clang -arch arm64 -O2 -Wall -Wno-deprecated-declarations -o vttest vttest.c $FW
codesign -s - -f vttest
file $B/MacOS/StelaVT
# The bundle hands its host encoder and decoder to these exports, and
# registers the factories.
for arch in $(lipo -archs $B/MacOS/StelaVT); do
  for symbol in _StelaVT_CreateInstance _StelaVT_UseEncoders _StelaVD_CreateInstance _StelaVD_UseDecoders; do
    nm -gU -arch $arch $B/MacOS/StelaVT | grep -q " $symbol\$" || { echo "StelaVT ($arch) exports no $symbol"; exit 1; }
  done
done
shasum -a 256 $B/MacOS/StelaVT
