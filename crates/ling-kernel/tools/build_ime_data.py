"""Build the LingOS IME dictionaries and the Daemon stacked font.

Outputs (into OUT, normally ling/crates/ling-kernel/assets/ime):
  ja_dict.z   zlib: sorted lines  reading \t cost \t surf1 \t surf2 ...      (mozc OSS dictionary)
  zh_dict.z   zlib: sorted lines  py'syl'lab \t simp|trad|tones \t ...       (CC-CEDICT + jieba freq)
  daemon_stacked.ttf  DaemonChesilin's Hangul-block glyphs at Plane-15 PUA code points

Run with python -I build_dicts.py <dl-dir> <out-dir> <ui-font.ttf> <DaemonChesilin.otf>
"""
import collections
import glob
import gzip
import math
import os
import re
import sys
import zlib

from fontTools.ttLib import TTFont
from fontTools.ttLib.tables import _m_a_x_p
from fontTools.misc import sstruct


def _maxp_dec(self, data, ttFont):
    # DaemonChesilin's maxp has 2 trailing bytes; fontTools asserts on them
    dummy, data2 = sstruct.unpack2(_m_a_x_p.maxpFormat_0_5, data, self)
    self.numGlyphs = int(self.numGlyphs)
    if self.tableVersion != 0x00005000:
        sstruct.unpack2(_m_a_x_p.maxpFormat_1_0_add, data2, self)


_m_a_x_p.table__m_a_x_p.decompile = _maxp_dec

DL, OUT, UIFONT, DAEMON = sys.argv[1:5]
os.makedirs(OUT, exist_ok=True)
covered = set(TTFont(UIFONT).getBestCmap().keys())


def font_has(s):
    return all(ord(c) in covered or ord(c) < 0x80 for c in s)


def hira(s):
    return all(('ぁ' <= c <= 'ゖ') or c == 'ー' for c in s)


# ── Japanese ──────────────────────────────────────────────────────────────
JA_MAX_COST = int(os.environ.get('JA_MAX_COST', '7400'))
JA_PER = 12
best = {}
for f in sorted(glob.glob(os.path.join(DL, 'dictionary0*.txt'))):
    for ln in open(f, encoding='utf-8'):
        p = ln.rstrip('\n').split('\t')
        if len(p) < 5:
            continue
        rd, cost, surf = p[0], int(p[3]), p[4]
        if not rd or len(rd) > 12 or not hira(rd):
            continue
        if not font_has(surf) or '\t' in surf or ' ' in surf:
            continue
        k = (rd, surf)
        if k not in best or cost < best[k]:
            best[k] = cost
by_rd = collections.defaultdict(list)
for (rd, surf), cost in best.items():
    by_rd[rd].append((cost, surf))
lines = []
n_ja = 0
for rd in sorted(by_rd):
    c = sorted(by_rd[rd])
    if c[0][0] > JA_MAX_COST:
        continue
    keep = [s for cost, s in c if cost <= JA_MAX_COST + 1500][:JA_PER]
    n_ja += len(keep)
    lines.append(rd + '\t' + str(c[0][0]) + '\t' + '\t'.join(keep))
ja = ('\n'.join(lines) + '\n').encode('utf-8')
open(os.path.join(OUT, 'ja_dict.z'), 'wb').write(zlib.compress(ja, 9))
print('ja: readings', len(lines), 'candidates', n_ja, 'raw', len(ja), 'z', len(zlib.compress(ja, 9)))

# ── Chinese ───────────────────────────────────────────────────────────────
SYL = re.compile(r'^[a-z]+[1-5]$')
freq = {}
for ln in open(os.path.join(DL, 'jieba_dict.txt'), encoding='utf-8'):
    p = ln.split()
    if len(p) >= 2:
        freq[p[0]] = int(p[1])
entries = []  # (simp, trad, [syls], tones)
for ln in gzip.open(os.path.join(DL, 'cedict.txt.gz'), 'rt', encoding='utf-8'):
    if ln.startswith('#'):
        continue
    m = re.match(r'^(\S+) (\S+) \[([^\]]+)\] /', ln)
    if not m:
        continue
    trad, simp, py = m.group(1), m.group(2), m.group(3)
    syls = py.lower().replace('u:', 'v').split()
    if len(syls) != len(simp) or not all(SYL.match(s) for s in syls):
        continue
    if not all('㐀' <= c <= '鿿' or '豈' <= c <= '﫿' for c in simp + trad):
        continue
    entries.append((simp, trad, [s[:-1] for s in syls], ''.join(s[-1] for s in syls)))
# share of each reading of a single char, from the words it appears in
share = collections.defaultdict(lambda: collections.defaultdict(float))
for simp, trad, syls, tones in entries:
    w = freq.get(simp, 0)
    if len(simp) > 1 and w:
        for ch, s in zip(simp, syls):
            share[ch][s] += w
out = collections.defaultdict(dict)  # key -> {(simp, trad): (score, tones)}
for simp, trad, syls, tones in entries:
    key = "'".join(syls)
    if len(simp) == 1:
        base = freq.get(simp, 0) + 1
        sh = share.get(simp)
        if sh:
            tot = sum(sh.values())
            part = sh.get(syls[0], 0.0) / tot if tot else 0.0
            score = base * max(part, 0.02)
        else:
            score = base
    else:
        f = freq.get(simp, 0)
        if f < 3:
            continue
        score = f
    if not (font_has(simp)):
        continue
    if not font_has(trad):
        trad = simp  # the UI font lacks a traditional form: offer the shared/simplified one
    k = (simp, trad)
    if k not in out[key] or out[key][k][0] < score:
        out[key][k] = (score, tones)
lines = []
n_zh = 0
miss_trad = set()
for key in sorted(out):
    c = sorted(out[key].items(), key=lambda kv: -kv[1][0])[:40]
    n_zh += len(c)
    for (s, t), _ in c:
        for ch in t:
            if ord(ch) not in covered:
                miss_trad.add(ch)
    lines.append(key + '\t' + str(int(math.log(c[0][1][0] + 1.0) * 100)) + '\t' + '\t'.join('%s|%s|%s' % (s, '' if t == s else t, tn) for (s, t), (sc, tn) in c))
zh = ('\n'.join(lines) + '\n').encode('utf-8')
open(os.path.join(OUT, 'zh_dict.z'), 'wb').write(zlib.compress(zh, 9))
print('zh: keys', len(lines), 'candidates', n_zh, 'raw', len(zh), 'z', len(zlib.compress(zh, 9)), 'trad chars missing from UI font', len(miss_trad))
open(os.path.join(OUT, '..', '..', 'ime_missing_trad.txt'), 'w', encoding='utf-8').write(''.join(sorted(miss_trad)))

# ── Daemon stacked font ───────────────────────────────────────────────────
f = TTFont(DAEMON)
cm = f.getBestCmap()
new = {}
for cp, g in cm.items():
    if 0xAC00 <= cp <= 0xD7A3:
        new[0xF0000 + cp - 0xAC00] = g
    elif 0x3131 <= cp <= 0x318E:
        new[0xF2C00 + cp - 0x3131] = g
new[0x20] = cm[0x20]
from fontTools.ttLib.tables._c_m_a_p import cmap_format_12, cmap_format_4
sub12 = cmap_format_12(12)
sub12.platformID, sub12.platEncID, sub12.language = 3, 10, 0
sub12.cmap = dict(new)
sub4 = cmap_format_4(4)
sub4.platformID, sub4.platEncID, sub4.language = 3, 1, 0
sub4.cmap = {0x20: cm[0x20]}
f['cmap'].tables = [sub4, sub12]
if 'GSUB' in f:
    del f['GSUB']
from fontTools import subset
opts = subset.Options()
opts.layout_features = []
opts.notdef_outline = True
opts.name_IDs = ['*']
sub = subset.Subsetter(opts)
sub.populate(unicodes=list(new.keys()))
sub.subset(f)
p = os.path.join(OUT, 'daemon_stacked.ttf')
f.save(p)
print('daemon_stacked.ttf', os.path.getsize(p), 'glyphs mapped', len(new))
