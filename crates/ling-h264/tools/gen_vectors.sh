#!/usr/bin/env bash
# Generates the bit-exactness test vectors with ffmpeg/libx264 (run inside WSL Arch):
#   wsl.exe -d Arch -- bash tools/gen_vectors.sh
# Writes tests/data/<name>.264 (Annex-B) + <name>.md5 (ffmpeg framemd5 of the yuv420p output)
# and the raw reference YUV to tests/data/yuv/<name>.yuv (git-ignored; only used for PSNR reports).
set -e
cd "$(dirname "$0")/../tests/data"
mkdir -p yuv
FF="ffmpeg -y -hide_banner -loglevel error"
gen() {
  name=$1; shift
  $FF "$@" -pix_fmt yuv420p -an -f h264 "$name.264"
  $FF -i "$name.264" -fps_mode passthrough -pix_fmt yuv420p -f framemd5 "$name.md5"
  $FF -i "$name.264" -fps_mode passthrough -pix_fmt yuv420p -f rawvideo "yuv/$name.yuv"
  echo "$name: $(stat -c %s "$name.264") bytes, $(grep -vc '^#' "$name.md5") frames"
}
T="-f lavfi -i testsrc2=size"
M="-f lavfi -i mandelbrot=size"
gen baseline_cavlc   $T=320x240:rate=25 -frames:v 30 -c:v libx264 -profile:v baseline -preset medium -crf 26
gen baseline_slices  $T=320x240:rate=25 -frames:v 20 -c:v libx264 -profile:v baseline -x264-params slices=4:ref=3 -crf 24
gen main_cabac_b     $T=352x288:rate=25 -frames:v 40 -c:v libx264 -profile:v main -x264-params bframes=3:b-pyramid=normal:ref=3 -crf 25
gen main_cavlc_b     $T=352x288:rate=25 -frames:v 30 -c:v libx264 -profile:v main -x264-params cabac=0:bframes=3:b-pyramid=normal:ref=3 -crf 25
gen high_8x8         $M=320x240:rate=25 -frames:v 30 -c:v libx264 -profile:v high -x264-params 8x8dct=1:bframes=2 -crf 24
gen high_cqm_jvt     $T=320x240:rate=25 -frames:v 30 -c:v libx264 -profile:v high -x264-params 8x8dct=1:cqm=jvt:bframes=2 -crf 24
gen high_cqm_custom  $M=320x240:rate=25 -frames:v 30 -c:v libx264 -profile:v high -x264-params "8x8dct=1:bframes=2:cqm4iy=8,12,18,26,12,18,26,30,18,26,30,36,26,30,36,40:cqm4py=14,16,20,24,16,20,24,28,20,24,28,32,24,28,32,36:cqm4ic=10,14,20,24,14,20,24,27,20,24,27,30,24,27,30,34:cqm8i=$(python3 -c 'print(",".join(str(8+(i%8)+(i//8)*2) for i in range(64)))'):cqm8p=$(python3 -c 'print(",".join(str(12+(i%8)*2+(i//8)) for i in range(64)))')" -crf 24
gen high_cavlc_8x8   $M=320x240:rate=25 -frames:v 30 -c:v libx264 -profile:v high -x264-params cabac=0:8x8dct=1:bframes=2 -crf 24
gen weightp          $T=352x288:rate=25,fade=in:0:25 -frames:v 40 -c:v libx264 -profile:v high -x264-params weightp=2:ref=4:bframes=0 -crf 24
gen weightb_implicit $T=352x288:rate=25,fade=in:0:25 -frames:v 40 -c:v libx264 -profile:v high -x264-params weightp=2:weightb=1:ref=4:bframes=3:b-pyramid=normal -crf 24
gen direct_temporal  $T=352x288:rate=25 -frames:v 40 -c:v libx264 -profile:v main -x264-params bframes=3:direct=temporal:b-pyramid=none:ref=2 -crf 26
gen direct_spatial   $M=352x288:rate=25 -frames:v 40 -c:v libx264 -profile:v high -x264-params bframes=3:direct=spatial:b-pyramid=strict:ref=3 -crf 26
gen direct_temporal_cavlc $T=352x288:rate=25 -frames:v 30 -c:v libx264 -profile:v main -x264-params cabac=0:bframes=2:direct=temporal:ref=3 -crf 24
gen partitions_all   $M=320x240:rate=25 -frames:v 30 -c:v libx264 -profile:v high -x264-params partitions=all:bframes=3:ref=3:8x8dct=1 -crf 22
gen slices_cabac     $T=352x288:rate=25 -frames:v 30 -c:v libx264 -profile:v high -x264-params slices=5:bframes=2 -crf 25
gen odd_426x240      $T=426x240:rate=25 -frames:v 30 -c:v libx264 -profile:v high -x264-params bframes=3 -crf 25
gen crop_640x360     $M=640x360:rate=25 -frames:v 20 -c:v libx264 -profile:v main -x264-params bframes=2:ref=2 -crf 28
gen nodeblock        $T=320x240:rate=25 -frames:v 20 -c:v libx264 -profile:v high -x264-params deblock=-3,2:bframes=2 -crf 30
gen lowqp            $M=320x240:rate=25 -frames:v 15 -c:v libx264 -profile:v high -x264-params bframes=2:8x8dct=1 -qp 4
gen highqp           $T=320x240:rate=25 -frames:v 20 -c:v libx264 -profile:v main -x264-params bframes=2 -qp 48
gen mixed_ref16      $M=352x288:rate=25 -frames:v 40 -c:v libx264 -profile:v high -x264-params ref=16:mixed-refs=1:bframes=3:b-adapt=2:me=umh:subme=9:trellis=2:keyint=15 -crf 22
gen constrained_intra $T=320x240:rate=25 -frames:v 20 -c:v libx264 -profile:v main -x264-params constrained-intra=1:bframes=0 -crf 25
gen youtube_like     $M=854x480:rate=30,fade=in:0:20,fade=out:40:20 -frames:v 60 -c:v libx264 -profile:v high -preset slow -x264-params bframes=3:b-pyramid=normal:weightp=2:ref=4:8x8dct=1:keyint=30 -crf 23
# yt_bbb_480p.264: first DASH segment of YouTube aqz-KE-bpKQ itag 135 (Big Buck Bunny, CC-BY Blender
# Foundation) converted to Annex-B; only its .md5 / yuv reference are generated here.
$FF -i yt_bbb_480p.264 -fps_mode passthrough -pix_fmt yuv420p -f framemd5 yt_bbb_480p.md5
$FF -i yt_bbb_480p.264 -fps_mode passthrough -pix_fmt yuv420p -f rawvideo yuv/yt_bbb_480p.yuv
# MP4 container sample for the avcC / length-prefixed path
$FF -f lavfi -i testsrc2=size=320x240:rate=25 -frames:v 30 -c:v libx264 -profile:v high -x264-params bframes=3 -crf 25 -pix_fmt yuv420p -movflags +faststart mp4_high.mp4
$FF -i mp4_high.mp4 -fps_mode passthrough -pix_fmt yuv420p -f framemd5 mp4_high.md5
$FF -i mp4_high.mp4 -fps_mode passthrough -pix_fmt yuv420p -f rawvideo yuv/mp4_high.yuv
# speed vectors (not committed: git-ignored in yuv/)
for r in 640x360 854x480 1280x720; do
  $FF -f lavfi -i testsrc2=size=$r:rate=30 -frames:v 120 -c:v libx264 -profile:v high -preset medium -crf 23 -pix_fmt yuv420p -an -f h264 yuv/speed_$r.264
done
du -sh . yuv
