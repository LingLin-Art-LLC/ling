/* Test-vector helper (host only, not part of the crate): encode raw s16le PCM
 * with libfdk-aac (HE-AAC / HE-AACv2, which FFmpeg's native encoder cannot
 * produce) and write
 *   out.aus  : u32le asc_len, asc bytes, then (u32le len, AU bytes)*  (raw AUs)
 *   out.adts : the same AUs with ADTS headers (for FFmpeg's reference decode)
 * Build: gcc -O2 fdkenc.c -lfdk-aac -o fdkenc
 * Usage: fdkenc in.s16 rate channels aot bitrate out.aus out.adts
 */
#include <fdk-aac/aacenc_lib.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void put32(FILE *f, unsigned v) { unsigned char b[4] = {v, v >> 8, v >> 16, v >> 24}; fwrite(b, 1, 4, f); }

static int sr_index(int rate) {
    static const int r[] = {96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350};
    for (int i = 0; i < 13; i++) if (r[i] == rate) return i;
    return 15;
}

int main(int argc, char **argv) {
    if (argc != 8) { fprintf(stderr, "usage\n"); return 1; }
    FILE *in = fopen(argv[1], "rb");
    int rate = atoi(argv[2]), ch = atoi(argv[3]), aot = atoi(argv[4]), br = atoi(argv[5]);
    FILE *aus = fopen(argv[6], "wb"), *adts = fopen(argv[7], "wb");
    HANDLE_AACENCODER h;
    if (aacEncOpen(&h, 0, ch) != AACENC_OK) return 2;
    aacEncoder_SetParam(h, AACENC_AOT, aot);
    aacEncoder_SetParam(h, AACENC_SAMPLERATE, rate);
    aacEncoder_SetParam(h, AACENC_CHANNELMODE, ch == 1 ? MODE_1 : MODE_2);
    aacEncoder_SetParam(h, AACENC_BITRATE, br);
    aacEncoder_SetParam(h, AACENC_TRANSMUX, TT_MP4_RAW);
    aacEncoder_SetParam(h, AACENC_SIGNALING_MODE, 2); /* explicit hierarchical */
    if (aacEncEncode(h, NULL, NULL, NULL, NULL) != AACENC_OK) return 3;
    AACENC_InfoStruct info;
    aacEncInfo(h, &info);
    put32(aus, info.confSize);
    fwrite(info.confBuf, 1, info.confSize, aus);
    /* core rate / channel config for ADTS */
    int core_rate = (aot == 5 || aot == 29) ? rate / 2 : rate;
    int adts_ch = (aot == 29) ? 1 : ch;
    int frame = info.frameLength;
    short *pcm = malloc(sizeof(short) * frame * ch);
    unsigned char out[20000];
    int eof = 0;
    for (;;) {
        AACENC_BufDesc ibuf = {0}, obuf = {0};
        AACENC_InArgs iargs = {0};
        AACENC_OutArgs oargs = {0};
        int n = 0;
        if (!eof) {
            n = fread(pcm, sizeof(short) * ch, frame, in);
            if (n <= 0) eof = 1;
        }
        void *ip = pcm, *op = out;
        int iid = IN_AUDIO_DATA, oid = OUT_BITSTREAM_DATA;
        int isz = n * ch * 2, osz = sizeof(out), iel = 2, oel = 1;
        ibuf.numBufs = 1; ibuf.bufs = &ip; ibuf.bufferIdentifiers = &iid; ibuf.bufSizes = &isz; ibuf.bufElSizes = &iel;
        obuf.numBufs = 1; obuf.bufs = &op; obuf.bufferIdentifiers = &oid; obuf.bufSizes = &osz; obuf.bufElSizes = &oel;
        iargs.numInSamples = eof ? -1 : n * ch;
        AACENC_ERROR e = aacEncEncode(h, &ibuf, &obuf, &iargs, &oargs);
        if (e == AACENC_ENCODE_EOF) break;
        if (e != AACENC_OK) { fprintf(stderr, "encode error %x\n", e); return 4; }
        if (oargs.numOutBytes > 0) {
            int len = oargs.numOutBytes;
            put32(aus, len);
            fwrite(out, 1, len, aus);
            int flen = len + 7;
            unsigned char h7[7];
            h7[0] = 0xFF; h7[1] = 0xF1;
            h7[2] = (1 << 6) | (sr_index(core_rate) << 2) | (adts_ch >> 2);
            h7[3] = ((adts_ch & 3) << 6) | (flen >> 11);
            h7[4] = (flen >> 3) & 0xFF;
            h7[5] = ((flen & 7) << 5) | 0x1F;
            h7[6] = 0xFC;
            fwrite(h7, 1, 7, adts);
            fwrite(out, 1, len, adts);
        }
    }
    fclose(aus); fclose(adts);
    aacEncClose(&h);
    return 0;
}
