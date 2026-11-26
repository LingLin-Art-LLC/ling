//! The Shannon stream cipher + MAC that Spotify's access point protocol uses
//! after the DH handshake (a port of the `shannon` crate librespot uses, which
//! is Qualcomm's reference code). Each AP packet: nonce = its sequence number
//! (big endian), encrypt header + payload, then a 4-byte MAC.

const N: usize = 16;
const INITKONST: u32 = 0x6996_c53a;
const KEYP: usize = 13;

#[derive(Clone)]
pub struct Shannon {
    r: [u32; N],
    crc: [u32; N],
    init_r: [u32; N],
    konst: u32,
    sbuf: u32,
    mbuf: u32,
    nbuf: u32,
}

#[inline]
fn rotl(w: u32, x: u32) -> u32 {
    w.rotate_left(x)
}
#[inline]
fn sbox1(mut w: u32) -> u32 {
    w ^= rotl(w, 5) | rotl(w, 7);
    w ^= rotl(w, 19) | rotl(w, 22);
    w
}
#[inline]
fn sbox2(mut w: u32) -> u32 {
    w ^= rotl(w, 7) | rotl(w, 22);
    w ^= rotl(w, 5) | rotl(w, 19);
    w
}

impl Shannon {
    pub fn new(key: &[u8]) -> Shannon {
        let mut c = Shannon { r: [0; N], crc: [0; N], init_r: [0; N], konst: INITKONST, sbuf: 0, mbuf: 0, nbuf: 0 };
        c.r[0] = 1;
        c.r[1] = 1;
        for i in 2..N {
            c.r[i] = c.r[i - 1].wrapping_add(c.r[i - 2]);
        }
        c.loadkey(key);
        c.konst = c.r[0];
        c.init_r = c.r;
        c
    }

    fn cycle(&mut self) {
        let mut t = self.r[12] ^ self.r[13] ^ self.konst;
        t = sbox1(t) ^ rotl(self.r[0], 1);
        for i in 1..N {
            self.r[i - 1] = self.r[i];
        }
        self.r[N - 1] = t;
        t = sbox2(self.r[2] ^ self.r[15]);
        self.r[0] ^= t;
        self.sbuf = t ^ self.r[8] ^ self.r[12];
    }

    fn diffuse(&mut self) {
        for _ in 0..N {
            self.cycle();
        }
    }

    fn loadkey(&mut self, key: &[u8]) {
        for w in key.chunks(4) {
            let mut x = [0u8; 4];
            x[..w.len()].copy_from_slice(w);
            self.r[KEYP] ^= u32::from_le_bytes(x);
            self.cycle();
        }
        self.r[KEYP] ^= key.len() as u32;
        self.cycle();
        self.crc = self.r;
        self.diffuse();
        for i in 0..N {
            self.r[i] ^= self.crc[i];
        }
    }

    pub fn nonce(&mut self, nonce: &[u8]) {
        self.r = self.init_r;
        self.konst = INITKONST;
        self.loadkey(nonce);
        self.konst = self.r[0];
        self.nbuf = 0;
    }

    pub fn nonce_u32(&mut self, n: u32) {
        self.nonce(&n.to_be_bytes());
    }

    fn crcfunc(&mut self, i: u32) {
        let t = self.crc[0] ^ self.crc[2] ^ self.crc[15] ^ i;
        for j in 1..N {
            self.crc[j - 1] = self.crc[j];
        }
        self.crc[N - 1] = t;
    }

    fn macfunc(&mut self, i: u32) {
        self.crcfunc(i);
        self.r[KEYP] ^= i;
    }

    fn process(&mut self, buf: &mut [u8], enc: bool) {
        let mut i = 0usize;
        if self.nbuf != 0 {
            while self.nbuf > 0 {
                if i >= buf.len() {
                    return;
                }
                let b = &mut buf[i];
                i += 1;
                if enc {
                    self.mbuf ^= (*b as u32) << (32 - self.nbuf);
                    *b ^= ((self.sbuf >> (32 - self.nbuf)) & 0xff) as u8;
                } else {
                    *b ^= ((self.sbuf >> (32 - self.nbuf)) & 0xff) as u8;
                    self.mbuf ^= (*b as u32) << (32 - self.nbuf);
                }
                self.nbuf -= 8;
            }
            let m = self.mbuf;
            self.macfunc(m);
        }
        let rest = &mut buf[i..];
        let whole = rest.len() & !3;
        let (words, extra) = rest.split_at_mut(whole);
        for w in words.chunks_mut(4) {
            self.cycle();
            let mut t = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
            if enc {
                self.macfunc(t);
                t ^= self.sbuf;
            } else {
                t ^= self.sbuf;
                self.macfunc(t);
            }
            w.copy_from_slice(&t.to_le_bytes());
        }
        if !extra.is_empty() {
            self.cycle();
            self.mbuf = 0;
            self.nbuf = 32;
            for b in extra.iter_mut() {
                if enc {
                    self.mbuf ^= (*b as u32) << (32 - self.nbuf);
                    *b ^= ((self.sbuf >> (32 - self.nbuf)) & 0xff) as u8;
                } else {
                    *b ^= ((self.sbuf >> (32 - self.nbuf)) & 0xff) as u8;
                    self.mbuf ^= (*b as u32) << (32 - self.nbuf);
                }
                self.nbuf -= 8;
            }
        }
    }

    pub fn encrypt(&mut self, buf: &mut [u8]) {
        self.process(buf, true)
    }
    pub fn decrypt(&mut self, buf: &mut [u8]) {
        self.process(buf, false)
    }

    pub fn finish(&mut self, out: &mut [u8]) {
        if self.nbuf != 0 {
            let m = self.mbuf;
            self.macfunc(m);
        }
        self.cycle();
        self.r[KEYP] ^= INITKONST ^ (self.nbuf << 3);
        self.nbuf = 0;
        for i in 0..N {
            self.r[i] ^= self.crc[i];
        }
        self.diffuse();
        for w in out.chunks_mut(4) {
            self.cycle();
            let b = self.sbuf.to_le_bytes();
            w.copy_from_slice(&b[..w.len()]);
        }
    }
}

fn hex(b: &[u8]) -> alloc::string::String {
    b.iter().map(|x| alloc::format!("{x:02x}")).collect()
}

/// Known-answer check against the reference `shannon` crate (vectors made on
/// the host with shannon 0.2.0): key 00..1f, two nonces, whole and partial words.
pub fn selftest() -> Result<(), alloc::string::String> {
    let key: alloc::vec::Vec<u8> = (0u8..32).collect();
    let mut c = Shannon::new(&key);
    c.nonce_u32(0);
    let mut buf = b"LingOS shannon vector, 37 bytes long".to_vec();
    c.encrypt(&mut buf);
    let mut mac = [0u8; 4];
    c.finish(&mut mac);
    let want0 = "8614874ccc7f37c1bc24b8e01b04923ebb5aa338c9061caf8d10c99e89656d91b3b6245c";
    if hex(&buf) != want0 || hex(&mac) != "b6f49fd1" {
        return Err(alloc::format!("vector 0: {} {}", hex(&buf), hex(&mac)));
    }
    c.nonce_u32(1);
    let mut b2 = [0xabu8; 3];
    c.encrypt(&mut b2);
    let mut b3 = [0x11u8; 9];
    c.encrypt(&mut b3);
    c.finish(&mut mac);
    if hex(&b2) != "103c8d" || hex(&b3) != "be19e30dd383702759" || hex(&mac) != "9cf4de3e" {
        return Err(alloc::format!("vector 1: {} {} {}", hex(&b2), hex(&b3), hex(&mac)));
    }
    // and back
    let mut d = Shannon::new(&key);
    d.nonce_u32(0);
    let mut p = hex_to(want0);
    d.decrypt(&mut p);
    let mut m2 = [0u8; 4];
    d.finish(&mut m2);
    if &p[..] != b"LingOS shannon vector, 37 bytes long" || hex(&m2) != "b6f49fd1" {
        return Err(alloc::string::String::from("decrypt does not invert encrypt"));
    }
    Ok(())
}

pub fn hex_to(s: &str) -> alloc::vec::Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}
