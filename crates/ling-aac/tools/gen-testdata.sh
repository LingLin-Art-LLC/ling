#!/usr/bin/env bash
# Generate AAC test vectors + FFmpeg float reference decodes for `cargo test -p ling-aac`.
# Needs ffmpeg, gcc and libfdk-aac (headers) -- e.g. WSL Arch:
#   wsl.exe -d Arch -- bash /mnt/c/.../crates/ling-aac/tools/gen-testdata.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/../tests/data"
mkdir -p "$out"
cd "$out"
ff="ffmpeg -hide_banner -loglevel error -y"
D=4

# --- source signals (stereo, distinct channels) ---------------------------
sweep="aevalsrc=exprs='0.5*sin(2*PI*(40*t+(19000-40)/(2*$D)*t*t))|0.4*sin(2*PI*(40*t+(19000-40)/(2*$D)*t*t)+1.3)'"
noise="anoisesrc=d=$D:c=pink:a=0.25:seed=1[a];anoisesrc=d=$D:c=white:a=0.15:seed=2[b];[a][b]amerge=inputs=2"
# chords with plucked envelopes, a bass line and hard clicks (forces short windows)
music="aevalsrc=exprs='0.25*exp(-4*mod(t,0.5))*(sin(2*PI*220*t)+0.6*sin(2*PI*277.18*t)+0.5*sin(2*PI*329.63*t)+0.3*sin(2*PI*440*t+sin(2*PI*5*t)))+0.2*sin(2*PI*(55+27.5*floor(mod(t*2,4)))*t)+0.6*lt(mod(t,0.25),0.0015)*sin(2*PI*3000*t)|0.25*exp(-4*mod(t+0.25,0.5))*(sin(2*PI*246.94*t)+0.6*sin(2*PI*293.66*t)+0.5*sin(2*PI*369.99*t))+0.2*sin(2*PI*(55+27.5*floor(mod(t*2,4)))*t)+0.5*lt(mod(t+0.1,0.25),0.001)'"

gen() { # name rate filter
  $ff -f lavfi -i "$3" -t $D -ar "$2" -ac 2 -c:a pcm_s16le -f s16le "$1.s16"
}
gen sweep44 44100 "$sweep:s=44100:d=$D"
gen noise44 44100 "$noise"
gen music44 44100 "$music:s=44100:d=$D"
gen music48 48000 "$music:s=48000:d=$D"

# --- AAC-LC with FFmpeg's native encoder ----------------------------------
lc() { # name src rate channels extra...
  local name=$1 src=$2 rate=$3 ch=$4; shift 4
  $ff -f s16le -ar "$rate" -ac 2 -i "$src.s16" -ac "$ch" -c:a aac -b:a 128k "$@" "$name.m4a"
  $ff -i "$name.m4a" -c:a pcm_f32le -f f32le "$name.ref.f32"
}
lc lc_sweep44 sweep44 44100 2
lc lc_noise44 noise44 44100 2
lc lc_music44 music44 44100 2
lc lc_music48 music48 48000 2
lc lc_music44_mono music44 44100 1
lc lc_music44_nopns music44 44100 2 -aac_pns 0 -aac_is 0
lc lc_music48_96k music48 48000 2 -b:a 96k
# YouTube-style DASH audio (itag 140 layout): ftyp, moov+mvex, global sidx, moof+mdat
$ff -f s16le -ar 48000 -ac 2 -i music48.s16 -c:a aac -b:a 128k     -movflags frag_keyframe+empty_moov+default_base_moof+global_sidx -frag_duration 1000000 lc_dash48.m4a
$ff -i lc_dash48.m4a -c:a pcm_f32le -f f32le lc_dash48.ref.f32

# --- HE-AAC / HE-AACv2 with libfdk-aac -------------------------------------
gcc -O2 "$here/fdkenc.c" -lfdk-aac -o "$out/fdkenc"
./fdkenc music44.s16 44100 2 5 64000 he_music44.aus he_music44.adts
./fdkenc sweep44.s16 44100 2 5 64000 he_sweep44.aus he_sweep44.adts
./fdkenc music48.s16 48000 2 5 64000 he_music48.aus he_music48.adts
$ff -f s16le -ar 44100 -ac 2 -i music44.s16 -ac 1 -f s16le music44m.s16
./fdkenc music44m.s16 44100 1 5 48000 he_music44_mono.aus he_music44_mono.adts
./fdkenc music44.s16 44100 2 29 32000 hev2_music44.aus hev2_music44.adts
for f in he_music44 he_sweep44 he_music48 he_music44_mono hev2_music44; do
  $ff -i "$f.adts" -c:a pcm_f32le -f f32le "$f.ref.f32"
done
# implicit SBR signalling: an LC-looking config at 22.05 kHz carrying SBR payloads
$ff -i he_music44.adts -c copy he_music44_implicit.m4a
rm -f fdkenc *.s16
ls -la
