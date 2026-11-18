#!/bin/sh
# Builds the FFmpeg the release binary carries, run inside env.sh: static libraries in /opt/ffmpeg with
# only what Sweeflow uses (HEVC decoding, on the GPU through VA-API or on the CPU, scale_vaapi, swscale).
# Built under the LGPL (no --enable-gpl, nothing nonfree), so carrying it is fine for an MIT program as
# long as its source and this configuration ship with the release (build.sh copies both next to the
# packages, and the licences page says where they are).
#
# Statically linked, the binary doesn't depend on the distro's FFmpeg, whose library versions change
# between releases (libavcodec 58 on Ubuntu 22.04, 60 on 24.04, 61 on Debian 13, 63 on Arch now), so one
# build works everywhere. It still uses the system's libva, libva-drm and libdrm, which keep their
# versions, so the GPU drivers are the system's own.
set -eu
VERSION=9.0.2
cd /root
[ -f ffmpeg-$VERSION.tar.xz ] || curl -fLO https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz
rm -rf ffmpeg-$VERSION && tar -xJf ffmpeg-$VERSION.tar.xz && cd ffmpeg-$VERSION
CONFIG="--prefix=/opt/ffmpeg --enable-static --disable-shared --enable-pic --disable-programs --disable-doc \
 --disable-everything --disable-autodetect --disable-network --disable-avformat --disable-avdevice --disable-swresample \
 --enable-avcodec --enable-avfilter --enable-swscale --enable-vaapi --enable-libdrm \
 --enable-decoder=hevc --enable-parser=hevc --enable-hwaccel=hevc_vaapi \
 --enable-filter=buffer,buffersink,scale_vaapi,format,null"
# shellcheck disable=SC2086
./configure $CONFIG
make -j"$(nproc)"
rm -rf /opt/ffmpeg && make install
mkdir -p /opt/ffmpeg/share/source
cp ../ffmpeg-$VERSION.tar.xz COPYING.LGPLv2.1 /opt/ffmpeg/share/source/
echo "./configure $CONFIG" > /opt/ffmpeg/share/source/CONFIGURE.txt
grep -E 'CONFIG_(VAAPI|GPL|NONFREE) ' config.h
grep -E 'CONFIG_(HEVC_VAAPI_HWACCEL|SCALE_VAAPI_FILTER|HEVC_DECODER) ' config_components.h
