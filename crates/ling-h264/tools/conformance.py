"""Run ling-h264 over JVT conformance bitstreams and compare with ffmpeg (bit-exact).

usage: python tools/conformance.py <dir with JVT .zip files> [name-filter]

Zips come from https://www.itu.int/wftp3/av-arch/jvt-site/draft_conformance/ (AVCv1, FRExt).
Needs: target/release/examples/h264dec(.exe)  (cargo build --release --example h264dec)
       ffmpeg (native, or in WSL Arch: `wsl.exe -d Arch -- ffmpeg`).
"""
import os, re, subprocess, sys, zipfile, shutil

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, '..', '..', '..'))
DEC = os.path.join(ROOT, 'target', 'release', 'examples', 'h264dec' + ('.exe' if os.name == 'nt' else ''))
EXTS = ('.264', '.h264', '.jsv', '.26l', '.avc', '.jvt', '.bit', '.bin', '.26L', '.JSV', '.AVC')


def wslpath(p):
    p = os.path.abspath(p).replace('\\', '/')
    if re.match(r'^[A-Za-z]:/', p):
        return '/mnt/' + p[0].lower() + p[2:]
    return p


def ffmpeg(args):
    if shutil.which('ffmpeg'):
        return subprocess.run(['ffmpeg'] + args, capture_output=True)
    args = [wslpath(a) if (os.sep in a or '/' in a) and os.path.exists(os.path.dirname(os.path.abspath(a))) else a for a in args]
    env = dict(os.environ, MSYS_NO_PATHCONV='1')
    return subprocess.run(['wsl.exe', '-d', 'Arch', '--', 'ffmpeg'] + args, capture_output=True, env=env)


def main():
    zdir = sys.argv[1]
    filt = sys.argv[2] if len(sys.argv) > 2 else ''
    work = os.path.join(zdir, '..', 'work')
    os.makedirs(work, exist_ok=True)
    results = []
    for z in sorted(os.listdir(zdir)):
        if not z.lower().endswith('.zip') or filt not in z:
            continue
        name = z[:-4]
        d = os.path.join(work, name)
        if not os.path.isdir(d):
            os.makedirs(d)
            try:
                zipfile.ZipFile(os.path.join(zdir, z)).extractall(d)
            except Exception as e:
                results.append((name, 'BADZIP', str(e)))
                continue
        streams = []
        for r, _, fs in os.walk(d):
            for f in fs:
                if f.endswith(EXTS) and not f.endswith('.yuv'):
                    streams.append(os.path.join(r, f))
        if not streams:
            results.append((name, 'NOSTREAM', ''))
            continue
        bs = max(streams, key=os.path.getsize)
        out = os.path.join(d, 'ling.yuv')
        ref = os.path.join(d, 'ffmpeg.yuv')
        p = subprocess.run([DEC, bs, out], capture_output=True, text=True)
        m = re.search(r'(\d+)x(\d+): (\d+) frames', p.stdout)
        errs = p.stderr.strip().replace('\n', ' ')[:120]
        if not os.path.exists(ref):
            ffmpeg(['-y', '-hide_banner', '-loglevel', 'error', '-i', bs, '-fps_mode', 'passthrough', '-pix_fmt', 'yuv420p', '-f', 'rawvideo', ref])
        a = open(out, 'rb').read() if os.path.exists(out) else b''
        b = open(ref, 'rb').read() if os.path.exists(ref) else b''
        if not m:
            results.append((name, 'NOOUTPUT', errs))
            continue
        w, h, n = int(m.group(1)), int(m.group(2)), int(m.group(3))
        fsz = w * h + 2 * ((w + 1) // 2) * ((h + 1) // 2)
        nref = len(b) // fsz if fsz else 0
        jvt = [os.path.join(r, f) for r, _, fs in os.walk(d) for f in fs
               if f.lower().endswith('.yuv') and f not in ('ling.yuv', 'ffmpeg.yuv')]
        jvt_ok = any(os.path.getsize(j) == len(a) and open(j, 'rb').read() == a for j in jvt) if a else False
        if jvt_ok and a != b:
            results.append((name, 'OK', '%dx%d %d frames (matches the JVT reference yuv; ffmpeg output differs)' % (w, h, n)))
        elif a == b and a:
            results.append((name, 'OK', '%dx%d %d frames%s' % (w, h, n, (' (decoder said: %s)' % errs) if errs else '')))
        else:
            bad = next((i for i in range(min(n, nref)) if a[i * fsz:(i + 1) * fsz] != b[i * fsz:(i + 1) * fsz]), None)
            results.append((name, 'DIFF', '%dx%d ours %d ffmpeg %d frames, first bad frame %s %s' % (w, h, n, nref, bad, errs)))
        print('%-24s %-8s %s' % results[-1], flush=True)
    ok = sum(1 for r in results if r[1] == 'OK')
    print('\n%d / %d bit-exact' % (ok, len(results)))
    for r in results:
        if r[1] != 'OK':
            print('%-24s %-8s %s' % r)


if __name__ == '__main__':
    main()
