#!/usr/bin/env bash
# Generate MP4 test files + ffprobe packet lists for `cargo test -p ling-mp4`.
# Needs ffmpeg/ffprobe with libx264, e.g. WSL Arch:
#   wsl.exe -d Arch -- bash /mnt/c/.../crates/ling-mp4/tools/gen-testdata.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/../tests/data"
mkdir -p "$out"
cd "$out"
ff="ffmpeg -hide_banner -loglevel error -y"
V="-f lavfi -i testsrc=size=320x240:rate=30 -f lavfi -i sine=frequency=440:sample_rate=44100 -t 3"
VENC="-c:v libx264 -preset veryfast -g 15 -bf 2 -pix_fmt yuv420p"
AENC="-c:a aac -b:a 96k"

$ff $V $VENC $AENC av.mp4                                                    # moov at the end
$ff $V $VENC $AENC -movflags +faststart av_faststart.mp4                     # moov first
$ff $V $VENC $AENC -movflags frag_keyframe+empty_moov+default_base_moof av_frag.mp4
$ff $V $VENC $AENC -movflags frag_keyframe+empty_moov av_frag_abs.mp4        # absolute base_data_offset
# DASH / YouTube style: one track per file, fragments + a global sidx
$ff -f lavfi -i testsrc=size=320x240:rate=30 -t 4 $VENC -movflags frag_keyframe+empty_moov+default_base_moof+global_sidx -frag_duration 1000000 v_dash.mp4
$ff -f lavfi -i sine=frequency=440:sample_rate=48000 -t 4 $AENC -movflags frag_keyframe+empty_moov+default_base_moof+global_sidx -frag_duration 1000000 a_dash.m4a
# ffmpeg's dash muxer (single file per representation, sidx per segment set)
rm -rf dashout && mkdir dashout
$ff $V $VENC $AENC -f dash -single_file 1 -seg_duration 1 -use_template 0 -use_timeline 0 dashout/manifest.mpd
cp dashout/*stream0*.m4s dash_single_v.mp4 2>/dev/null || cp dashout/*-stream0.* dash_single_v.mp4
cp dashout/*stream1*.m4s dash_single_a.mp4 2>/dev/null || cp dashout/*-stream1.* dash_single_a.mp4
rm -rf dashout

for f in av.mp4 av_faststart.mp4 av_frag.mp4 av_frag_abs.mp4 v_dash.mp4 a_dash.m4a dash_single_v.mp4 dash_single_a.mp4; do
  ffprobe -v error -ignore_editlist 1 -show_entries packet=stream_index,pts,dts,duration,size,pos,flags -of csv=p=0 "$f" > "$f.packets.csv"
  ffprobe -v error -show_entries stream=index,codec_name,width,height,sample_rate,channels,time_base,extradata -show_data -of flat "$f" > "$f.streams.txt" 2>/dev/null || true
done
ls -la
