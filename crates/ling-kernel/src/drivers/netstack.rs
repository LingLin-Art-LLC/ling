//! Minimal IPv4/TCP/HTTP client stack over the (now actually working --
//! see net_e1000's module doc) e1000 driver: enough real protocol to GET
//! a file from a real HTTP server, which is what `lingfu` needs to stop
//! being local-only. Static-IP SLIRP defaults (10.0.2.15/24, gateway
//! 10.0.2.2 -- which QEMU aliases to the host's loopback, so a plain
//! `python -m http.server` on the host is a reachable package repo).
//!
//! Honest scope, per driver-idiom: one TCP connection at a time, client
//! only, out-of-order segments reassembled via a small reorder buffer (a
//! duplicate ACK prompts the peer to fast-retransmit the gap), no local
//! retransmit queue (requests are sent once; against SLIRP's local, lossless
//! link this is fine and is disclosed rather than hidden), no window
//! management beyond a fixed
//! receive window, no TLS (a public HTTPS repo needs a real TLS stack --
//! see packages/README.md; the repo URL is plain HTTP by design until
//! then). This is a real wire-protocol implementation, not a simulation:
//! every byte below goes through the e1000 ring to a real peer.

use crate::arch::timer;
// The NIC: the e1000 or the Intel I225/I226 (igc) on x86 (`drivers::netdev`
// picks), the RP1's GEM on the Raspberry Pi 5.
#[cfg(target_arch = "x86_64")]
use crate::drivers::netdev as nic;
#[cfg(target_arch = "aarch64")]
use crate::arch::netdev as nic;

/// Live IP config, resolved through the NIC's runtime config (a DHCP lease
/// if one was obtained, else the SLIRP fallback) — never the compile-time
/// const, so a fresh lease takes effect for every packet we build.
pub fn self_ip() -> [u8; 4] {
    nic::self_ip()
}
pub fn gateway_ip() -> [u8; 4] {
    nic::gateway_ip()
}
const MSS: usize = 1400;
// Advertised receive window. Sized to let several MSS-sized segments be in
// flight per round trip (large bodies -- the ~84KiB package avatars, bigger
// web pages -- otherwise trickle one window per RTT). 32KiB: fits the TLS rx
// buffer (64KiB) with room to spare, and stays under the e1000 RX ring's
// capacity (32 descriptors x 2KiB = 64KiB) so a brief non-drain can't drop
// segments. Halves the round trips for an 84KiB image versus a 16KiB window.
const RX_WINDOW: u16 = 32768;
/// Kernel-time poll budget. This bounds how long a *failed* fetch blocks
/// the (single-threaded) desktop loop -- a healthy fetch returns as soon
/// as the data arrives, well under this. Kept modest (8s) because the
/// whole UI is frozen while it runs: a dead host or an https redirect
/// must not hang the machine. Healthy DNS/TCP complete in well under a
/// second in practice (verified: curl example.com over the real
/// internet), so this only bites on genuine failure.
const WAIT_BUDGET_US: u64 = 8_000_000;

// -- Cooperative net locks (multiplexed) ------------------------------------
// Three levels, all cooperative (context switches only happen at yield points,
// so a test-and-set with no yield between is atomic):
//
//  * EXCLUSIVE (`NetGuard`, `net_acquire`): the old whole-stack lock, for the
//    code that still uses state with one slot -- the legacy client socket
//    (`CLIENT_HANDLE`), tls.rs' static buffers, DHCP, SNTP. Re-entrant per task.
//  * SHARED (`NetShare`): a transaction on sockets of its own (handle API,
//    lingsock streams, Horizon's pages and pictures, WebSockets). Any number of
//    tasks hold it at once -- their sockets' traffic interleaves on the wire,
//    every pump files each frame into the socket (or UDP mailbox) it belongs
//    to. Excludes EXCLUSIVE both ways; an exclusive waiter blocks NEW shared
//    claims so it cannot starve.
//  * IO (`IoGuard`): a short critical section around touching the NIC and the
//    socket table (a pump, a send). Holders may yield inside (the Pi's SDIO
//    radio waits), so it is a real re-entrant mutex; it never waits on the two
//    above except "not while another task holds EXCLUSIVE".
//
// Either claim (`net_owned_by_me`) is what lets a green thread's network waits
// hand the CPU back to the desktop (timer::poll_until); `net_held` keeps the
// Wi-Fi tick off the radio while any transaction is in flight.
static mut NET_OWNER: i64 = -1;
static mut NET_DEPTH: u32 = 0;
const MAX_SHARERS: usize = 32;
/// (task, depth) of every shared claim; task -1 = free slot.
static mut SHARERS: [(i64, u32); MAX_SHARERS] = [(-1, 0); MAX_SHARERS];
static mut SHARE_COUNT: u32 = 0;
static mut EXCL_WAITING: u32 = 0;
static mut IO_OWNER: i64 = -1;
static mut IO_DEPTH: u32 = 0;

fn net_spin() {
    crate::proc::sched::yield_now();
    crate::coro::yield_now();
}

fn share_slot(me: i64) -> Option<usize> {
    unsafe { (0..MAX_SHARERS).find(|&i| SHARERS[i].0 == me) }
}
/// Shared claims held by tasks other than `me`.
fn others_sharing(me: i64) -> bool {
    let mine = share_slot(me).map(|i| unsafe { SHARERS[i].1 }).unwrap_or(0);
    unsafe { SHARE_COUNT > mine }
}

/// Acquire the exclusive net lock (re-entrant per task). Yields until no other
/// task holds it or a shared claim.
pub fn net_acquire() {
    let me = crate::coro::task_key();
    unsafe {
        if NET_OWNER == me {
            NET_DEPTH += 1;
            return;
        }
        let upgrade = share_slot(me).is_some();
        let t0 = timer::now_ms();
        let mut warned = false;
        EXCL_WAITING += 1;
        while (NET_OWNER != -1 && NET_OWNER != me) || others_sharing(me) {
            // A shared holder asking for exclusive waits for every other shared
            // holder; two doing it at once would wait on each other forever.
            // Nothing in the shared paths should (it is logged), and after a
            // while the upgrade goes ahead rather than hang the machine.
            if upgrade && NET_OWNER == -1 {
                if !warned {
                    warned = true;
                    crate::console_write(b"net: exclusive lock asked inside a shared claim by ");
                    crate::console_write(crate::coro::current_name());
                    crate::console_write(b"\n");
                }
                if timer::now_ms().saturating_sub(t0) > 15_000 {
                    break;
                }
            }
            net_spin();
        }
        EXCL_WAITING -= 1;
        NET_OWNER = me;
        NET_DEPTH += 1;
    }
}

/// Release one level of the exclusive net lock.
pub fn net_release() {
    unsafe {
        if NET_DEPTH > 0 {
            NET_DEPTH -= 1;
        }
        if NET_DEPTH == 0 {
            NET_OWNER = -1;
        }
    }
}

/// Take a shared claim (see above). Re-entrant; the exclusive holder may take
/// one too (it changes nothing for it).
pub fn net_share() {
    let me = crate::coro::task_key();
    unsafe {
        if let Some(i) = share_slot(me) {
            SHARERS[i].1 += 1;
            SHARE_COUNT += 1;
            return;
        }
        if NET_OWNER != me {
            while (NET_OWNER != -1 && NET_OWNER != me) || EXCL_WAITING > 0 {
                net_spin();
            }
        }
        loop {
            if let Some(i) = (0..MAX_SHARERS).find(|&i| SHARERS[i].0 == -1) {
                SHARERS[i] = (me, 1);
                SHARE_COUNT += 1;
                return;
            }
            net_spin(); // 32 tasks mid-transaction: wait for a slot
        }
    }
}

pub fn net_unshare() {
    let me = crate::coro::task_key();
    unsafe {
        if let Some(i) = share_slot(me) {
            SHARERS[i].1 -= 1;
            SHARE_COUNT = SHARE_COUNT.saturating_sub(1);
            if SHARERS[i].1 == 0 {
                SHARERS[i] = (-1, 0);
            }
        }
    }
}

/// Does the running cooperative task hold a net claim (either kind)? This is
/// the gate for `poll_until`'s coroutine yield: a green thread hands main the
/// CPU only inside its own network transaction, never mid disk I/O
/// (interleaved ATA / lingfs commits between a coroutine and main would
/// corrupt the FS).
pub fn net_owned_by_me() -> bool {
    let me = crate::coro::task_key();
    unsafe { (NET_OWNER != -1 && NET_OWNER == me) || share_slot(me).is_some() }
}
/// Is any cooperative task in a network transaction right now? The Wi-Fi tick
/// on main uses this to stay off the SDIO bus while a green-thread fetch is
/// driving the radio.
pub fn net_held() -> bool {
    unsafe { NET_OWNER != -1 || SHARE_COUNT > 0 }
}
/// Does another task hold the exclusive lock (so the stack must be left alone)?
pub fn net_busy_for_me() -> bool {
    unsafe { NET_OWNER != -1 && NET_OWNER != crate::coro::task_key() }
}
/// A green thread was killed (its app quit, `coro::kill_owner`): every claim it
/// still held -- exclusive, shared, IO, DNS -- goes, or the next network call
/// from anyone spins forever waiting for a task that no longer exists (an app
/// quit mid-request froze its next launch on the loading screen). Returns how
/// many claims were dropped.
pub fn forget_task(key: i64) -> u32 {
    let mut n = 0;
    unsafe {
        if NET_OWNER == key {
            NET_OWNER = -1;
            NET_DEPTH = 0;
            n += 1;
        }
        for i in 0..MAX_SHARERS {
            if SHARERS[i].0 == key {
                SHARE_COUNT = SHARE_COUNT.saturating_sub(SHARERS[i].1);
                SHARERS[i] = (-1, 0);
                n += 1;
            }
        }
        if IO_OWNER == key {
            IO_OWNER = -1;
            IO_DEPTH = 0;
            n += 1;
        }
        if DNS_OWNER == key {
            DNS_OWNER = -1;
            n += 1;
        }
    }
    n
}

/// The sockets a killed green thread had open (its `Stream`s died with its
/// stack, nobody will close them): freed at once, no FIN -- the table has
/// only MAX_SOCKS slots and every quit mid-fetch used to keep one forever.
pub fn free_task_sockets(key: i64) -> u32 {
    let _io = IoGuard::new();
    let mut n = 0;
    for i in 0..MAX_SOCKS {
        if unsafe { SOCKS[i].state != SockState::Free && SOCKS[i].task == key } {
            if unsafe { CLIENT_HANDLE } == i {
                unsafe { CLIENT_HANDLE = usize::MAX };
            }
            sock_free(i);
            n += 1;
        }
    }
    n
}

/// Socket `handle` changes hands: `Some(key)` -- that task has it now (a
/// pooled keep-alive connection picked up by a new request); None -- no task
/// (parked in the keep-alive pool, so killing the task that put it there
/// cannot free it under the pool).
pub fn adopt_socket(handle: usize, key: Option<i64>) {
    if handle < MAX_SOCKS {
        unsafe { SOCKS[handle].task = key.unwrap_or(-1) };
    }
}

/// How many tasks are in a shared transaction (the `net` console line).
pub fn net_sharers() -> u32 {
    unsafe { (0..MAX_SHARERS).filter(|&i| SHARERS[i].0 != -1).count() as u32 }
}

fn io_acquire() {
    let me = crate::coro::task_key();
    unsafe {
        if IO_OWNER == me {
            IO_DEPTH += 1;
            return;
        }
        while (IO_OWNER != -1 && IO_OWNER != me) || (NET_OWNER != -1 && NET_OWNER != me) {
            net_spin();
        }
        IO_OWNER = me;
        IO_DEPTH += 1;
    }
}
fn io_release() {
    unsafe {
        if IO_DEPTH > 0 {
            IO_DEPTH -= 1;
        }
        if IO_DEPTH == 0 {
            IO_OWNER = -1;
        }
    }
}

/// RAII guard for `net_acquire`/`net_release` (exclusive).
pub struct NetGuard;
impl NetGuard {
    pub fn new() -> Self {
        net_acquire();
        NetGuard
    }
}
impl Drop for NetGuard {
    fn drop(&mut self) {
        net_release();
    }
}

/// RAII shared claim: a transaction on sockets of its own.
pub struct NetShare;
impl NetShare {
    pub fn new() -> Self {
        net_share();
        NetShare
    }
}
impl Drop for NetShare {
    fn drop(&mut self) {
        net_unshare();
    }
}

/// RAII IO critical section (the NIC and the socket table).
pub(crate) struct IoGuard;
impl IoGuard {
    pub(crate) fn new() -> Self {
        io_acquire();
        IoGuard
    }
}
impl Drop for IoGuard {
    fn drop(&mut self) {
        io_release();
    }
}

static mut GW_MAC: [u8; 6] = [0; 6];
static mut GW_MAC_KNOWN: bool = false;
/// Timestamp (ms) of the last *failed* gateway ARP, or 0 for "never failed".
/// Without this, a dead gateway made every `dns_query` re-run the 8-second
/// `arp_selftest` — three servers per resolve froze the desktop for ~24s.
/// With it, one failure suppresses further ARP attempts for a cool-off
/// window, so an offline machine fails a lookup in well under a second while
/// still recovering automatically once the network comes back.
static mut GW_FAIL_AT_MS: u64 = 0;
/// How long to trust a cached ARP failure before trying the gateway again.
const GW_FAIL_COOLOFF_MS: u64 = 20_000;

fn checksum(data: &[u8], init: u32) -> u16 {
    let mut sum = init;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Resolve (and cache) the gateway MAC via ARP. Brings the NIC up first
/// if nothing else has yet (idempotent). A recent failure is cached (see
/// `GW_FAIL_AT_MS`) so a down network doesn't re-eat the 8s ARP timeout on
/// every call.
pub fn arp_gateway() -> Option<[u8; 6]> {
    if !nic::ensure_init() {
        return None;
    }
    unsafe {
        if GW_MAC_KNOWN {
            return Some(GW_MAC);
        }
        // Suppress re-ARP during the cool-off after a recent failure.
        if GW_FAIL_AT_MS != 0 && timer::now_ms().wrapping_sub(GW_FAIL_AT_MS) < GW_FAIL_COOLOFF_MS {
            return None;
        }
    }
    // On the Pi over Wi-Fi, the wired-only `nic::arp_selftest` (RP1 Ethernet
    // GEM) can't reach the gateway, so every off-subnet fetch (DNS, HTTPS --
    // Horizon, lingfu, wallet, outbound SSH) failed after a successful join.
    // There, resolve the gateway over the ACTIVE device via `arp_resolve`
    // (which sends/receives through `nic::{mac,transmit,receive}` = Wi-Fi when
    // joined). x86 and a wired Pi keep `nic::arp_selftest`, which uses an 8s
    // budget tuned for QEMU's fast-TSC RX delivery -- don't regress them.
    let _io = IoGuard::new();
    unsafe {
        if GW_MAC_KNOWN {
            return Some(GW_MAC); // found while we waited for the NIC
        }
    }
    let result = {
        #[cfg(target_arch = "aarch64")]
        {
            if crate::wifi::joined() {
                let gw = gateway_ip();
                if gw == [0, 0, 0, 0] {
                    return None; // no gateway yet (pre-DHCP / link-local)
                }
                arp_resolve(gw)
            } else {
                nic::arp_selftest()
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            nic::arp_selftest()
        }
    };
    match result {
        Some(mac) => unsafe {
            GW_MAC = mac;
            GW_MAC_KNOWN = true;
            GW_FAIL_AT_MS = 0;
            Some(mac)
        },
        None => unsafe {
            GW_FAIL_AT_MS = timer::now_ms().max(1);
            None
        },
    }
}

/// Drop the cached gateway MAC (and any failure cool-off) so the next send
/// re-ARPs. Called after a new IP config (DHCP lease / static IP) lands, in
/// case the gateway changed.
pub fn invalidate_gateway() {
    unsafe {
        GW_MAC_KNOWN = false;
        GW_FAIL_AT_MS = 0;
        (*(&raw mut DNS_CACHE)).clear();
    }
}

// Resolved names, so a page's many sub-resources on one host pay one lookup
// (each uncached lookup is a UDP round trip, and a slow resolver cost a second
// or more per request). Entries expire after DNS_TTL_MS; a new IP / DNS
// configuration clears them. Only touched under the net lock's cooperative
// scheduling, like the rest of this file's statics.
static mut DNS_CACHE: alloc::vec::Vec<(alloc::string::String, [u8; 4], u64)> = alloc::vec::Vec::new();
const DNS_TTL_MS: u64 = 300_000;
const DNS_CACHE_MAX: usize = 32;

fn dns_cache_get(host: &str) -> Option<[u8; 4]> {
    let now = timer::now_ms();
    let cache = unsafe { &*(&raw const DNS_CACHE) };
    cache
        .iter()
        .find(|(h, _, at)| h.eq_ignore_ascii_case(host) && now.saturating_sub(*at) < DNS_TTL_MS)
        .map(|&(_, ip, _)| ip)
}

fn dns_cache_put(host: &str, ip: [u8; 4]) {
    let cache = unsafe { &mut *(&raw mut DNS_CACHE) };
    let now = timer::now_ms();
    cache.retain(|(h, _, at)| !h.eq_ignore_ascii_case(host) && now.saturating_sub(*at) < DNS_TTL_MS);
    if cache.len() >= DNS_CACHE_MAX {
        cache.remove(0);
    }
    cache.push((alloc::string::String::from(host), ip, now));
}

/// Is `ip` on our own IPv4 subnet (per the current mask)?
fn same_subnet(ip: [u8; 4]) -> bool {
    let me = self_ip();
    let mask = nic::subnet_mask();
    (0..4).all(|i| (me[i] & mask[i]) == (ip[i] & mask[i]))
}

/// Ethernet next-hop MAC for `dst_ip`: ARP a same-subnet peer directly (LAN
/// peer-to-peer -- two LingOS machines on the same segment), otherwise route
/// via the gateway (the internet/SLIRP case). Broadcast/all-ones stays a
/// direct broadcast. The TCP layer routes every segment through this so a
/// same-subnet peer is reachable even on a segment with no gateway.
fn next_hop_mac(dst_ip: [u8; 4]) -> Option<[u8; 6]> {
    if dst_ip == [255, 255, 255, 255] {
        return Some([0xFF; 6]);
    }
    if same_subnet(dst_ip) {
        arp_resolve(dst_ip)
    } else {
        arp_gateway()
    }
}

// -- Multi-socket TCP ---------------------------------------------------------
// Several concurrent connections over the one NIC: a reserved client socket
// (the legacy `tcp_connect`/`tcp_write`/`tcp_read_*`/`tcp_close` API, used by
// TLS/browser/lingfu -- unchanged for those callers) plus server/accepted
// sockets via the handle API (`tcp_listen_accept` -> handle, `tcp_*_h`). A
// single RX pump (`pump`) reads the shared NIC queue and fans each packet to
// the matching socket by 4-tuple (or a listening socket by local port), so an
// SSH session and an HTTPS fetch can run at the same time. Crucially, reads and
// writes take the net lock only briefly (around one pump/segment), never for a
// whole session -- so flows interleave instead of one starving the others,
// which the old whole-session lock + single `CONN` couldn't do.

const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_ACK: u8 = 0x10;
const TCP_PSH: u8 = 0x08;

const MAX_SOCKS: usize = 24;
const SOCK_RX_CAP: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum SockState {
    Free,
    Listen,
    SynSent,
    SynRcvd,
    Established,
    Closed,
}

struct Sock {
    state: SockState,
    remote_ip: [u8; 4],
    remote_port: u16,
    local_port: u16,
    snd_nxt: u32,
    rcv_nxt: u32,
    rx: [u8; SOCK_RX_CAP],
    rx_len: usize,
    got_fin: bool,
    /// A passively-opened (accepted) socket stays `Established` for the life of
    /// the connection, so `tcp_listen_accept` would keep re-matching and
    /// re-returning it. Set once it's been handed to a caller so the accept
    /// scan skips it thereafter -- otherwise a second "accept" runs a second
    /// handshake on the LIVE socket and closes it.
    accepted: bool,
    /// Oldest sequence number the peer has not acknowledged yet.
    snd_una: u32,
    /// The peer's advertised receive window (bytes we may have in flight).
    snd_wnd: u32,
    /// Duplicate ACKs seen for `snd_una` (three trigger a fast retransmit).
    dupacks: u8,
    /// When the socket was created (half-open sockets time out).
    born_ms: u64,
    /// The receive window we last advertised (a window update follows when
    /// the application frees space after it had shrunk).
    last_adv: u32,
    /// The cooperative task that opened it (coro::task_key): a green thread
    /// killed with its app frees its sockets (`forget_task`).
    task: i64,
    /// Largest segment the peer takes (its SYN's MSS option, at most our MSS): a router that
    /// clamps it (PPPoE, a tunnel) drops anything bigger.
    snd_mss: u16,
    /// The peer's window-scale shift (RFC 7323; only when both SYNs carried the option):
    /// without it a sender never has more than 64 KB in flight.
    snd_shift: u8,
    /// Congestion window and slow-start threshold (bytes; Reno with NewReno recovery): the
    /// sender no longer bursts its whole queue into a home router, loses half and stalls.
    cwnd: u32,
    ssthresh: u32,
    /// Fast recovery: on until everything sent before the loss (`recover`) is acknowledged.
    in_recovery: bool,
    recover: u32,
    /// Diagnostics for a stalled send: segments from the peer, when the last one came.
    rx_segs: u32,
    last_rx_ms: u64,
}

/// Frames the NIC refused to send (its ring full) and frames received, for the
/// send-gave-up line.
static mut TX_FAILS: u32 = 0;
static mut RX_FRAMES: u32 = 0;

// Unacknowledged segments per socket, resent on timeout or after three
// duplicate ACKs; a socket whose oldest segment fails TX_MAX_TRIES times is
// declared dead (the peer rebooted or left), so callers notice instead of
// waiting forever on a half-open connection.
// 64 segments = ~90 KB in flight: with 16 (22 KB) an upload to a server 100+ ms away
// crawled, and one loss emptied the pipe.
const TXQ_N: usize = 64;
const TX_MAX_TRIES: u8 = 8;

#[derive(Clone, Copy)]
struct TxSeg {
    used: bool,
    seq: u32,
    flags: u8,
    len: usize,
    sent_ms: u64,
    tries: u8,
    data: [u8; MSS],
}

const EMPTY_TXSEG: TxSeg = TxSeg { used: false, seq: 0, flags: 0, len: 0, sent_ms: 0, tries: 0, data: [0; MSS] };
static mut TXQ: [[TxSeg; TXQ_N]; MAX_SOCKS] = [[EMPTY_TXSEG; TXQ_N]; MAX_SOCKS];

const EMPTY_SOCK: Sock = Sock {
    state: SockState::Free,
    remote_ip: [0; 4],
    remote_port: 0,
    local_port: 0,
    snd_nxt: 0,
    rcv_nxt: 0,
    rx: [0; SOCK_RX_CAP],
    rx_len: 0,
    got_fin: false,
    accepted: false,
    snd_una: 0,
    snd_wnd: 0,
    dupacks: 0,
    born_ms: 0,
    last_adv: 0,
    task: -1,
    snd_mss: MSS as u16,
    snd_shift: 0,
    cwnd: (10 * MSS) as u32,
    ssthresh: u32::MAX / 2,
    in_recovery: false,
    recover: 0,
    rx_segs: 0,
    last_rx_ms: 0,
};

static mut SOCKS: [Sock; MAX_SOCKS] = [const { EMPTY_SOCK }; MAX_SOCKS];
/// The socket the legacy (implicit) client API operates on. `usize::MAX` = none.
static mut CLIENT_HANDLE: usize = usize::MAX;

fn sock_alloc() -> Option<usize> {
    unsafe {
        for i in 0..MAX_SOCKS {
            if SOCKS[i].state == SockState::Free {
                let s = &mut SOCKS[i];
                s.remote_ip = [0; 4];
                s.remote_port = 0;
                s.local_port = 0;
                s.snd_nxt = 0;
                s.rcv_nxt = 0;
                s.rx_len = 0;
                s.got_fin = false;
                s.accepted = false;
                s.snd_una = 0;
                s.snd_wnd = RX_WINDOW as u32;
                s.dupacks = 0;
                s.born_ms = timer::now_ms();
                s.task = crate::coro::task_key();
                s.snd_mss = MSS as u16;
                s.snd_shift = 0;
                s.cwnd = (10 * MSS) as u32;
                s.ssthresh = u32::MAX / 2;
                s.in_recovery = false;
                s.recover = 0;
                s.rx_segs = 0;
                s.last_rx_ms = 0;
                txq_clear(i);
                return Some(i);
            }
        }
    }
    None
}

fn sock_free(idx: usize) {
    if idx >= MAX_SOCKS {
        return;
    }
    ooo_reset_sock(idx);
    txq_clear(idx);
    unsafe {
        SOCKS[idx].state = SockState::Free;
        SOCKS[idx].rx_len = 0;
        SOCKS[idx].got_fin = false;
    }
}

/// `a` is at or after `b` in sequence space (mod 2^32).
fn seq_ge(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) < 0x8000_0000
}

fn txq_clear(idx: usize) {
    if idx >= MAX_SOCKS {
        return;
    }
    unsafe {
        for s in (&mut *&raw mut TXQ)[idx].iter_mut() {
            s.used = false;
        }
    }
}

fn txq_count(idx: usize) -> usize {
    unsafe { (&*&raw const TXQ)[idx].iter().filter(|s| s.used).count() }
}

/// Remember a segment that consumes sequence space until it is acknowledged.
fn txq_push(idx: usize, seq: u32, flags: u8, data: &[u8]) {
    unsafe {
        let q = &mut (&mut *&raw mut TXQ)[idx];
        if let Some(s) = q.iter_mut().find(|s| !s.used) {
            s.used = true;
            s.seq = seq;
            s.flags = flags;
            s.len = data.len().min(MSS);
            s.data[..s.len].copy_from_slice(&data[..s.len]);
            s.sent_ms = timer::now_ms();
            s.tries = 0;
        }
    }
}

/// Sequence space a queued segment covers (SYN and FIN count one each).
fn seg_span(s: &TxSeg) -> u32 {
    s.len as u32 + (s.flags & (TCP_SYN | TCP_FIN) != 0) as u32
}

/// An ACK arrived: drop what it covers, count duplicates, take the window.
/// `win` is already scaled (the peer's shift applied; a SYN's own window never is).
fn on_ack(idx: usize, ack: u32, win: u32, payload_len: usize) {
    unsafe {
        let s = &mut SOCKS[idx];
        let win_changed = s.snd_wnd != win;
        s.snd_wnd = win;
        let mss = (s.snd_mss as u32).max(64);
        let cap = (TXQ_N as u32) * mss;
        if seq_ge(ack, s.snd_una) && ack != s.snd_una && seq_ge(s.snd_nxt, ack) {
            let acked = ack.wrapping_sub(s.snd_una);
            s.snd_una = ack;
            s.dupacks = 0;
            for q in (&mut *&raw mut TXQ)[idx].iter_mut() {
                if q.used && seq_ge(ack, q.seq.wrapping_add(seg_span(q))) {
                    q.used = false;
                }
            }
            if s.in_recovery {
                if seq_ge(ack, s.recover) {
                    // everything sent before the loss is in: recovery is over
                    s.in_recovery = false;
                    s.cwnd = s.ssthresh.max(2 * mss);
                } else {
                    // a partial ACK (NewReno): the next hole is lost too, resend it now
                    // instead of sitting out a timeout for every lost segment
                    s.cwnd = s.cwnd.saturating_sub(acked).max(2 * mss) + mss;
                    retransmit_oldest(idx);
                }
            } else if s.cwnd < s.ssthresh {
                s.cwnd = s.cwnd.saturating_add(acked.min(mss));
            } else {
                s.cwnd = s.cwnd.saturating_add((mss * mss / s.cwnd.max(1)).max(1));
            }
            s.cwnd = s.cwnd.min(cap);
        } else if ack == s.snd_una && payload_len == 0 && !win_changed && txq_count(idx) > 0 {
            s.dupacks = s.dupacks.saturating_add(1);
            if s.dupacks == 3 && !s.in_recovery {
                let flight = s.snd_nxt.wrapping_sub(s.snd_una);
                s.ssthresh = (flight / 2).max(2 * mss);
                s.cwnd = (s.ssthresh + 3 * mss).min(cap);
                s.in_recovery = true;
                s.recover = s.snd_nxt;
                retransmit_oldest(idx);
            } else if s.dupacks > 3 && s.in_recovery {
                // each further duplicate: one more segment has left the network
                s.cwnd = s.cwnd.saturating_add(mss).min(cap);
            }
        }
    }
}

/// What the socket may have in flight now: the peer's window and the congestion window.
fn send_room(s: &Sock) -> u32 {
    s.snd_wnd.min(s.cwnd).max(s.snd_mss as u32)
}

/// The TCP options of a SYN: (MSS, window-scale shift).
fn syn_options(opts: &[u8]) -> (Option<u16>, Option<u8>) {
    let (mut mss, mut ws) = (None, None);
    let mut i = 0;
    while i < opts.len() {
        match opts[i] {
            0 => break,
            1 => i += 1,
            k => {
                if i + 1 >= opts.len() {
                    break;
                }
                let l = opts[i + 1] as usize;
                if l < 2 || i + l > opts.len() {
                    break;
                }
                if k == 2 && l == 4 {
                    mss = Some(u16::from_be_bytes([opts[i + 2], opts[i + 3]]));
                } else if k == 3 && l == 3 {
                    ws = Some(opts[i + 2].min(14));
                }
                i += l;
            },
        }
    }
    (mss, ws)
}

fn oldest_seg(idx: usize) -> Option<usize> {
    let q = unsafe { &(&*&raw const TXQ)[idx] };
    let una = unsafe { SOCKS[idx].snd_una };
    // A segment the peer took only PART of (it trims what overruns its window and ACKs the
    // middle) starts before snd_una: `seq - una` wraps to ~4 G and sorted it as the newest,
    // so the retransmit timer resent a later segment forever while the peer waited for the
    // bytes from snd_una -- an upload deadlocked. Such a segment is the oldest: distance 0.
    (0..TXQ_N)
        .filter(|&k| q[k].used)
        .min_by_key(|&k| if seq_ge(q[k].seq, una) { q[k].seq.wrapping_sub(una) } else { 0 })
}

fn retransmit_oldest(idx: usize) {
    let Some(k) = oldest_seg(idx) else { return };
    let (seq, flags, len, data) = unsafe {
        let s = &mut (&mut *&raw mut TXQ)[idx][k];
        s.sent_ms = timer::now_ms();
        s.tries = s.tries.saturating_add(1);
        (s.seq, s.flags, s.len, s.data)
    };
    sock_send_seq(idx, flags, seq, &data[..len]);
}

/// Retransmission timeouts and half-open clean-up; runs on every pump.
fn tcp_timers() {
    let now = timer::now_ms();
    for idx in 0..MAX_SOCKS {
        let st = unsafe { SOCKS[idx].state };
        match st {
            SockState::Free | SockState::Listen => continue,
            _ => {},
        }
        if let Some(k) = oldest_seg(idx) {
            let (sent, tries) = unsafe {
                let s = &(&*&raw const TXQ)[idx][k];
                (s.sent_ms, s.tries)
            };
            let rto = (300u64 << tries.min(4)).min(4000);
            if now.wrapping_sub(sent) >= rto {
                // a timeout: the network lost a lot -- start again slowly (Reno)
                unsafe {
                    let s = &mut SOCKS[idx];
                    let mss = (s.snd_mss as u32).max(64);
                    s.ssthresh = (s.snd_nxt.wrapping_sub(s.snd_una) / 2).max(2 * mss);
                    s.cwnd = 2 * mss;
                    s.in_recovery = false;
                }
                // a peer whose window is shut (a busy server) is not gone: the probes go on
                let persist = unsafe { SOCKS[idx].snd_wnd } == 0 && st == SockState::Established;
                if tries >= TX_MAX_TRIES && !persist {
                    // the peer is gone: fail the socket so its owner notices
                    unsafe { SOCKS[idx].state = SockState::Closed };
                    txq_clear(idx);
                } else {
                    retransmit_oldest(idx);
                }
            }
        }
        // a half-open passive socket that never completed, or one that died
        // before anyone accepted it: nobody holds it, free it
        let listener = unsafe {
            let lp = SOCKS[idx].local_port;
            (0..MAX_SOCKS).any(|i| SOCKS[i].state == SockState::Listen && SOCKS[i].local_port == lp)
        };
        let (born, accepted) = unsafe { (SOCKS[idx].born_ms, SOCKS[idx].accepted) };
        let age = now.wrapping_sub(born);
        if listener && !accepted && ((st == SockState::SynRcvd && age > 8_000) || (st == SockState::Closed && age > 10_000)) {
            sock_free(idx);
        }
    }
}

/// A fresh initial sequence number (hardware random where there is a source).
fn new_isn() -> u32 {
    let mut b = [0u8; 4];
    if crate::crypto::random_bytes(&mut b) {
        return u32::from_le_bytes(b);
    }
    static mut CTR: u32 = 0x4C49_4E47;
    unsafe {
        CTR = CTR.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) ^ (timer::now_us() as u32);
        CTR
    }
}

/// A random ephemeral port no socket is using.
fn new_local_port() -> u16 {
    for _ in 0..32 {
        let p = 49152 + (new_isn() % 16000) as u16;
        let used = unsafe { (0..MAX_SOCKS).any(|i| SOCKS[i].state != SockState::Free && SOCKS[i].local_port == p) };
        if !used {
            return p;
        }
    }
    49152 + (timer::now_ms() % 16000) as u16
}

/// RST is honoured only inside the receive window (a blind attacker on the
/// LAN can no longer kill a chat by guessing the port).
fn rst_acceptable(idx: usize, flags: u8, seq: u32, ack: u32) -> bool {
    unsafe {
        let s = &SOCKS[idx];
        match s.state {
            SockState::SynSent => flags & TCP_ACK != 0 && ack == s.snd_nxt,
            _ => seq.wrapping_sub(s.rcv_nxt) < RX_WINDOW as u32,
        }
    }
}

/// Build + transmit one TCP segment for socket `idx`. The advertised window is
/// that socket's remaining receive-buffer space (real flow control), capped at
/// `RX_WINDOW`.
fn sock_send(idx: usize, flags: u8, payload: &[u8]) -> bool {
    let seq = unsafe { SOCKS[idx].snd_nxt };
    sock_send_seq(idx, flags, seq, payload)
}

/// Like `sock_send` at an explicit sequence number (retransmissions).
fn sock_send_seq(idx: usize, flags: u8, seq: u32, payload: &[u8]) -> bool {
    let (remote_ip, remote_port, local_port, rcv_nxt, win) = unsafe {
        let s = &mut SOCKS[idx];
        let free = SOCK_RX_CAP.saturating_sub(s.rx_len);
        let win = free.min(RX_WINDOW as usize) as u16;
        s.last_adv = win as u32;
        (s.remote_ip, s.remote_port, s.local_port, s.rcv_nxt, win)
    };
    raw_tcp_send(remote_ip, remote_port, local_port, seq, rcv_nxt, flags, win, payload)
}

/// One TCP segment with every header field given (also the RST answer to a
/// segment that matches no socket).
#[allow(clippy::too_many_arguments)]
fn raw_tcp_send(remote_ip: [u8; 4], remote_port: u16, local_port: u16, snd_nxt: u32, rcv_nxt: u32, flags: u8, win: u16, payload: &[u8]) -> bool {
    // Next hop: a same-subnet peer (LAN peer-to-peer, e.g. two LingOS on a
    // QEMU socket segment) is ARP'd directly; anything off-subnet goes via the
    // gateway. This used to always use the gateway MAC, which can't reach a
    // peer on a segment that has no gateway.
    // 127.0.0.0/8 and our own address never touch the wire: the frame goes
    // round the loopback ring (`loop_push`) and the next pump routes it back in
    // (a 127.x frame carries 127.x as its source too, so the reply comes back
    // to the same 4-tuple).
    let lo = is_local_dst(remote_ip);
    let src_ip = if lo && remote_ip[0] == 127 { remote_ip } else { self_ip() };
    let dmac = if lo {
        nic::mac()
    } else {
        let Some(m) = next_hop_mac(remote_ip) else { return false };
        m
    };
    // our SYN offers window scaling (shift 0: our own window stays under 64 KB) so the peer
    // may scale its window; a SYN-ACK answers with MSS only (we never ask the peer to scale)
    let opt_len = if flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {
        8
    } else if flags & TCP_SYN != 0 {
        4
    } else {
        0
    };
    let tcp_len = 20 + opt_len + payload.len();
    let ip_len = 20 + tcp_len;
    let mut f = [0u8; 14 + 20 + 20 + 8 + MSS];
    let total = 14 + ip_len;
    f[0..6].copy_from_slice(&dmac);
    f[6..12].copy_from_slice(&nic::mac());
    f[12..14].copy_from_slice(&[0x08, 0x00]);
    let ip = &mut f[14..];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
    ip[4..6].copy_from_slice(&0x4C49u16.to_be_bytes());
    ip[6] = 0x40;
    ip[8] = 64;
    ip[9] = 6;
    ip[12..16].copy_from_slice(&src_ip);
    ip[16..20].copy_from_slice(&remote_ip);
    let ipsum = checksum(&ip[..20], 0);
    ip[10..12].copy_from_slice(&ipsum.to_be_bytes());
    let t = &mut ip[20..];
    t[0..2].copy_from_slice(&local_port.to_be_bytes());
    t[2..4].copy_from_slice(&remote_port.to_be_bytes());
    t[4..8].copy_from_slice(&snd_nxt.to_be_bytes());
    t[8..12].copy_from_slice(&rcv_nxt.to_be_bytes());
    t[12] = (((20 + opt_len) / 4) as u8) << 4;
    t[13] = flags;
    t[14..16].copy_from_slice(&win.to_be_bytes());
    if opt_len > 0 {
        t[20..24].copy_from_slice(&[0x02, 0x04, (MSS >> 8) as u8, (MSS & 0xFF) as u8]);
    }
    if opt_len == 8 {
        t[24..28].copy_from_slice(&[0x01, 0x03, 0x03, 0x00]);
    }
    t[20 + opt_len..20 + opt_len + payload.len()].copy_from_slice(payload);
    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&src_ip);
    pseudo[4..8].copy_from_slice(&remote_ip);
    pseudo[9] = 6;
    pseudo[10..12].copy_from_slice(&(tcp_len as u16).to_be_bytes());
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < pseudo.len() {
        sum += u16::from_be_bytes([pseudo[i], pseudo[i + 1]]) as u32;
        i += 2;
    }
    let tsum = checksum(&ip[20..20 + tcp_len], sum);
    ip[20 + 16..20 + 18].copy_from_slice(&tsum.to_be_bytes());
    if lo {
        return loop_push(&f[..total]);
    }
    let ok = nic::transmit(&f[..total.max(60)]);
    if !ok {
        unsafe { TX_FAILS = TX_FAILS.wrapping_add(1) };
    }
    ok
}

// -- Out-of-order reassembly (per socket) -------------------------------------
// A small shared reorder pool, each slot tagged with its owning socket, so a
// large multi-segment flight (e.g. a TLS certificate chain) survives the normal
// reordering of the real internet instead of stalling on a retransmit timeout.
const OOO_SLOTS: usize = 24;
const OOO_SEG: usize = 1600;

struct OooSeg {
    used: bool,
    sock: usize,
    seq: u32,
    len: usize,
    data: [u8; OOO_SEG],
}

static mut OOO: [OooSeg; OOO_SLOTS] = [const {
    OooSeg { used: false, sock: 0, seq: 0, len: 0, data: [0; OOO_SEG] }
}; OOO_SLOTS];

fn ooo_reset_sock(idx: usize) {
    unsafe {
        for s in (&mut *&raw mut OOO).iter_mut() {
            if s.sock == idx {
                s.used = false;
            }
        }
    }
}

fn ooo_store(idx: usize, seq: u32, data: &[u8]) {
    if data.is_empty() || data.len() > OOO_SEG {
        return;
    }
    unsafe {
        let o = &mut *&raw mut OOO;
        for s in o.iter() {
            if s.used && s.sock == idx && s.seq == seq {
                return;
            }
        }
        for s in o.iter_mut() {
            if !s.used {
                s.used = true;
                s.sock = idx;
                s.seq = seq;
                s.len = data.len();
                s.data[..data.len()].copy_from_slice(data);
                return;
            }
        }
    }
}

/// Deliver any buffered segments now contiguous with the socket's `rcv_nxt` into
/// its receive buffer. Stops if the receive buffer is full (leaves the rest
/// buffered, unacked-advanced -- the reader drains and we resume next pump).
fn ooo_drain(idx: usize) {
    unsafe {
        let o = &mut *&raw mut OOO;
        loop {
            let expected = SOCKS[idx].rcv_nxt;
            let mut found = None;
            for (k, s) in o.iter().enumerate() {
                if s.used && s.sock == idx && s.seq == expected {
                    found = Some(k);
                    break;
                }
            }
            let Some(k) = found else { break };
            let len = o[k].len;
            let free = SOCK_RX_CAP.saturating_sub(SOCKS[idx].rx_len);
            if len > free {
                break; // no room yet; keep it buffered
            }
            let off = SOCKS[idx].rx_len;
            SOCKS[idx].rx[off..off + len].copy_from_slice(&o[k].data[..len]);
            SOCKS[idx].rx_len += len;
            SOCKS[idx].rcv_nxt = expected.wrapping_add(len as u32);
            o[k].used = false;
        }
    }
}

/// Process one received data segment into socket `idx`'s receive buffer:
/// in-order data is appended (+ any now-contiguous buffered data) and ACKed;
/// future data is buffered with a dup-ACK; a retransmit re-ACKs (delivering any
/// still-missing tail). Mirrors the old single-socket `recv_segment`.
fn recv_into(idx: usize, seq: u32, data: &[u8]) {
    unsafe {
        let expected = SOCKS[idx].rcv_nxt;
        if seq == expected {
            let free = SOCK_RX_CAP.saturating_sub(SOCKS[idx].rx_len);
            let take = data.len().min(free);
            let off = SOCKS[idx].rx_len;
            SOCKS[idx].rx[off..off + take].copy_from_slice(&data[..take]);
            SOCKS[idx].rx_len += take;
            SOCKS[idx].rcv_nxt = expected.wrapping_add(take as u32);
            if take == data.len() {
                ooo_drain(idx);
            }
            sock_send(idx, TCP_ACK, &[]);
        } else if seq_ge(seq, expected) {
            ooo_store(idx, seq, data);
            sock_send(idx, TCP_ACK, &[]);
        } else {
            let end = seq.wrapping_add(data.len() as u32);
            if seq_ge(end, expected) {
                let skip = expected.wrapping_sub(seq) as usize;
                if skip < data.len() {
                    let tail = &data[skip..];
                    let free = SOCK_RX_CAP.saturating_sub(SOCKS[idx].rx_len);
                    let take = tail.len().min(free);
                    let off = SOCKS[idx].rx_len;
                    SOCKS[idx].rx[off..off + take].copy_from_slice(&tail[..take]);
                    SOCKS[idx].rx_len += take;
                    SOCKS[idx].rcv_nxt = expected.wrapping_add(take as u32);
                    if take == tail.len() {
                        ooo_drain(idx);
                    }
                }
            }
            sock_send(idx, TCP_ACK, &[]);
        }
    }
}

/// Advance one connected socket's state machine for a received segment.
fn handle_conn(idx: usize, flags: u8, seq: u32, ack: u32, win: u16, payload: &[u8]) {
    unsafe {
        if flags & TCP_RST != 0 {
            if rst_acceptable(idx, flags, seq, ack) {
                SOCKS[idx].state = SockState::Closed;
                txq_clear(idx);
            }
            return;
        }
        let wnd = if flags & TCP_SYN != 0 { win as u32 } else { (win as u32) << SOCKS[idx].snd_shift };
        if flags & TCP_ACK != 0 && SOCKS[idx].state != SockState::SynSent {
            on_ack(idx, ack, wnd, payload.len());
        }
        match SOCKS[idx].state {
            SockState::SynSent => {
                if flags & TCP_SYN != 0 && flags & TCP_ACK != 0 && ack == SOCKS[idx].snd_nxt {
                    SOCKS[idx].rcv_nxt = seq.wrapping_add(1);
                    on_ack(idx, ack, wnd, 0);
                    SOCKS[idx].state = SockState::Established;
                    sock_send(idx, TCP_ACK, &[]);
                }
            },
            SockState::SynRcvd => {
                if flags & TCP_ACK != 0 && ack == SOCKS[idx].snd_nxt {
                    SOCKS[idx].state = SockState::Established;
                }
                if SOCKS[idx].state == SockState::Established && !payload.is_empty() {
                    recv_into(idx, seq, payload);
                }
            },
            SockState::Established => {
                if !payload.is_empty() {
                    recv_into(idx, seq, payload);
                }
                // a FIN counts only once everything before it has arrived: one that overtakes a
                // lost segment (a real network reorders; QEMU's SLIRP never does) would end the
                // stream early and lose that data -- the peer sends the FIN again with the gap
                if flags & TCP_FIN != 0 && seq.wrapping_add(payload.len() as u32) == SOCKS[idx].rcv_nxt {
                    SOCKS[idx].rcv_nxt = SOCKS[idx].rcv_nxt.wrapping_add(1);
                    sock_send(idx, TCP_ACK | TCP_FIN, &[]);
                    SOCKS[idx].got_fin = true;
                    SOCKS[idx].state = SockState::Closed;
                }
            },
            _ => {},
        }
    }
}

/// Route one received Ethernet frame to the matching socket (or a listener).
// A one-slot inbox for UDP datagrams to a single registered port (the
// Messenger discovery beacon). `route_packet` stashes them here so the ONE
// task that pumps the NIC (the messenger task) captures beacons as a side
// effect of servicing TCP/ARP -- the desktop's draw no longer needs to poll
// the NIC itself, which was racing that task for the shared receive queue and
// dropping frames (a lost SYN-ACK led to a duplicate connection that closed
// the live chat socket). Single slot is fine: beacons repeat every 2s.
static mut UDP_INBOX_PORT: u16 = 0;
const UDP_SLOTS: usize = 16;
static mut UDP_INBOX_IP: [[u8; 4]; UDP_SLOTS] = [[0; 4]; UDP_SLOTS];
static mut UDP_INBOX_BUF: [[u8; 512]; UDP_SLOTS] = [[0; 512]; UDP_SLOTS];
static mut UDP_INBOX_LEN: [usize; UDP_SLOTS] = [0; UDP_SLOTS];
static mut UDP_HEAD: usize = 0; // next to read
static mut UDP_COUNT: usize = 0;

/// One more UDP port whose datagrams go to a callback (src ip, src port,
/// payload) as they are routed -- lingspot's mDNS responder on 5353. The NIC
/// takes multicast already (e1000 RCTL.MPE).
static mut UDP_TAP: Option<(u16, fn([u8; 4], u16, &[u8]))> = None;

/// UDP mailboxes: a port someone is polling (`udp_poll_from` -- a DNS lookup,
/// DHCP, a time query) gets its datagrams kept here by WHOEVER pumps the NIC.
/// Before, the frame went to the task that happened to dequeue it, and any
/// other task's pump dropped it: with several fetches in flight, DNS answers
/// were lost and every lookup sat out its timeout.
const UDP_WANT_N: usize = 8;
const UDP_MAIL_N: usize = 16;
static mut UDP_WANT: [(u16, u64); UDP_WANT_N] = [(0, 0); UDP_WANT_N];
struct UdpMail {
    port: u16,
    ip: [u8; 4],
    len: usize,
    buf: [u8; 576],
}
static mut UDP_MAIL: [UdpMail; UDP_MAIL_N] = [const { UdpMail { port: 0, ip: [0; 4], len: 0, buf: [0; 576] } }; UDP_MAIL_N];

fn udp_want(port: u16) {
    let until = timer::now_ms() + 10_000;
    unsafe {
        if let Some(w) = UDP_WANT.iter_mut().find(|w| w.0 == port) {
            w.1 = until;
            return;
        }
        let now = timer::now_ms();
        let slot = (0..UDP_WANT_N).find(|&i| UDP_WANT[i].0 == 0 || UDP_WANT[i].1 < now).unwrap_or(0);
        UDP_WANT[slot] = (port, until);
    }
}
fn udp_wanted(port: u16) -> bool {
    let now = timer::now_ms();
    unsafe { UDP_WANT.iter().any(|w| w.0 == port && w.1 >= now) }
}
fn udp_mail_put(port: u16, ip: [u8; 4], data: &[u8]) {
    unsafe {
        let slot = (0..UDP_MAIL_N).find(|&i| UDP_MAIL[i].port == 0).unwrap_or_else(|| {
            // full: the oldest goes (slot order is arrival order only roughly; good enough)
            0
        });
        let m = &mut UDP_MAIL[slot];
        let n = data.len().min(m.buf.len());
        m.buf[..n].copy_from_slice(&data[..n]);
        m.len = n;
        m.ip = ip;
        m.port = port;
    }
}
fn udp_mail_take(port: u16, out: &mut [u8]) -> Option<([u8; 4], usize)> {
    unsafe {
        let i = (0..UDP_MAIL_N).find(|&i| UDP_MAIL[i].port == port)?;
        let m = &mut UDP_MAIL[i];
        let n = m.len.min(out.len());
        out[..n].copy_from_slice(&m.buf[..n]);
        m.port = 0;
        Some((m.ip, n))
    }
}

pub fn udp_tap(port: u16, f: Option<fn([u8; 4], u16, &[u8])>) {
    unsafe { UDP_TAP = f.map(|f| (port, f)) };
}

/// Send a UDP datagram to an IPv4 multicast group (its 01:00:5e MAC).
pub fn udp_multicast(group: [u8; 4], dst_port: u16, src_port: u16, payload: &[u8]) -> bool {
    if !nic::ensure_init() {
        return false;
    }
    let mac = [0x01, 0x00, 0x5e, group[1] & 0x7f, group[2], group[3]];
    // link-local multicast (mDNS) goes out with TTL 255 (RFC 6762 11)
    build_and_send_udp_ttl(mac, group, dst_port, src_port, payload, 255)
}

/// Register the UDP port whose datagrams `route_packet` should capture into
/// the inbox (read with [`udp_recv_beacon`]).
pub fn udp_listen(port: u16) {
    unsafe { UDP_INBOX_PORT = port };
}

/// Take the last captured datagram for the registered port, if any. Does not
/// touch the NIC -- safe to call from the UI thread without racing the pump.
pub fn udp_recv_beacon(out: &mut [u8]) -> Option<([u8; 4], usize)> {
    unsafe {
        if UDP_COUNT == 0 {
            return None;
        }
        let k = UDP_HEAD;
        let n = UDP_INBOX_LEN[k].min(out.len());
        out[..n].copy_from_slice(&UDP_INBOX_BUF[k][..n]);
        UDP_HEAD = (UDP_HEAD + 1) % UDP_SLOTS;
        UDP_COUNT -= 1;
        Some((UDP_INBOX_IP[k], n))
    }
}

fn route_packet(buf: &[u8]) {
    if buf.len() < 14 || martian(buf) {
        return;
    }
    // ARP (ethertype 0x0806): answer requests for our own IP so a LAN peer can
    // reach us with no gateway/SLIRP to proxy ARP on our behalf, and learn the
    // sender's MAC either way. Without this, two LingOS machines on a bare L2
    // segment can discover each other (broadcast beacons) but never open a TCP
    // connection (unicast, needs the peer's MAC).
    if buf[12] == 0x08 && buf[13] == 0x06 {
        handle_arp(buf);
        return;
    }
    // IPv4 UDP to the registered discovery port -> stash in the one-slot inbox.
    if buf.len() >= 42 && buf[12] == 0x08 && buf[13] == 0x00 && buf[23] == 17 {
        let ihl = ((buf[14] & 0x0F) as usize) * 4;
        let u = 14 + ihl;
        // a tapped port (lingspot's mDNS responder): handed over whole
        if let Some((tport, f)) = unsafe { UDP_TAP } {
            if u + 8 <= buf.len() && u16::from_be_bytes([buf[u + 2], buf[u + 3]]) == tport {
                let ulen = u16::from_be_bytes([buf[u + 4], buf[u + 5]]) as usize;
                let n = ulen.saturating_sub(8).min(buf.len() - u - 8);
                let mut src = [0u8; 4];
                src.copy_from_slice(&buf[26..30]);
                f(src, u16::from_be_bytes([buf[u], buf[u + 1]]), &buf[u + 8..u + 8 + n]);
                return;
            }
        }
        if u + 8 <= buf.len() && udp_wanted(u16::from_be_bytes([buf[u + 2], buf[u + 3]])) {
            let ulen = u16::from_be_bytes([buf[u + 4], buf[u + 5]]) as usize;
            let n = ulen.saturating_sub(8).min(buf.len() - u - 8);
            let mut src = [0u8; 4];
            src.copy_from_slice(&buf[26..30]);
            udp_mail_put(u16::from_be_bytes([buf[u + 2], buf[u + 3]]), src, &buf[u + 8..u + 8 + n]);
            return;
        }
        let port = unsafe { UDP_INBOX_PORT };
        if port != 0 && u + 8 <= buf.len() && u16::from_be_bytes([buf[u + 2], buf[u + 3]]) == port {
            let ulen = u16::from_be_bytes([buf[u + 4], buf[u + 5]]) as usize;
            let n = ulen.saturating_sub(8).min(buf.len() - u - 8);
            unsafe {
                if UDP_COUNT == UDP_SLOTS {
                    // full: drop the oldest
                    UDP_HEAD = (UDP_HEAD + 1) % UDP_SLOTS;
                    UDP_COUNT -= 1;
                }
                let k = (UDP_HEAD + UDP_COUNT) % UDP_SLOTS;
                let m = n.min(512);
                UDP_INBOX_BUF[k][..m].copy_from_slice(&buf[u + 8..u + 8 + m]);
                UDP_INBOX_LEN[k] = m;
                UDP_INBOX_IP[k].copy_from_slice(&buf[26..30]);
                UDP_COUNT += 1;
            }
            return;
        }
    }
    if buf.len() < 54 || buf[12] != 0x08 || buf[13] != 0x00 || buf[23] != 6 {
        return; // not IPv4/TCP
    }
    let ihl = ((buf[14] & 0x0F) as usize) * 4;
    let ip_total = u16::from_be_bytes([buf[16], buf[17]]) as usize;
    let mut src_ip = [0u8; 4];
    src_ip.copy_from_slice(&buf[26..30]);
    let t = 14 + ihl;
    if t + 20 > buf.len() {
        return;
    }
    let src_port = u16::from_be_bytes([buf[t], buf[t + 1]]);
    let dst_port = u16::from_be_bytes([buf[t + 2], buf[t + 3]]);
    let seq = u32::from_be_bytes([buf[t + 4], buf[t + 5], buf[t + 6], buf[t + 7]]);
    let ack = u32::from_be_bytes([buf[t + 8], buf[t + 9], buf[t + 10], buf[t + 11]]);
    let doff = ((buf[t + 12] >> 4) as usize) * 4;
    let flags = buf[t + 13];
    let win = u16::from_be_bytes([buf[t + 14], buf[t + 15]]);
    let payload_off = t + doff;
    let payload_end = (14 + ip_total).min(buf.len());
    let payload: &[u8] = if payload_end > payload_off {
        &buf[payload_off..payload_end]
    } else {
        &[]
    };

    // Existing connection (any non-Free, non-Listen socket) by 4-tuple.
    let mut idx = None;
    unsafe {
        for i in 0..MAX_SOCKS {
            let s = &SOCKS[i];
            if s.state != SockState::Free
                && s.state != SockState::Listen
                && s.remote_ip == src_ip
                && s.remote_port == src_port
                && s.local_port == dst_port
            {
                idx = Some(i);
                break;
            }
        }
    }
    let syn_opts = if flags & TCP_SYN != 0 && doff > 20 && t + doff <= buf.len() { syn_options(&buf[t + 20..t + doff]) } else { (None, None) };
    if let Some(i) = idx {
        unsafe {
            SOCKS[i].rx_segs = SOCKS[i].rx_segs.wrapping_add(1);
            SOCKS[i].last_rx_ms = timer::now_ms();
        }
        if flags & TCP_SYN != 0 && flags & TCP_ACK != 0 && unsafe { SOCKS[i].state } == SockState::SynSent {
            unsafe {
                if let Some(m) = syn_opts.0 {
                    SOCKS[i].snd_mss = m.clamp(256, MSS as u16);
                }
                // only our active open offers scaling, so only then does the peer's shift count
                SOCKS[i].snd_shift = syn_opts.1.unwrap_or(0);
            }
        }
        handle_conn(i, flags, seq, ack, win, payload);
        return;
    }
    let mut dst_ip = [0u8; 4];
    dst_ip.copy_from_slice(&buf[30..34]);
    let to_us = dst_ip_is_us(buf) || dst_ip[0] == 127;
    if to_us && flags & TCP_RST == 0 && !(flags & TCP_SYN != 0 && flags & TCP_ACK == 0) {
        // a segment for a connection we do not have (we rebooted, or it was
        // closed): reset it, so the peer stops waiting on a dead socket
        let span = payload.len() as u32 + (flags & (TCP_SYN | TCP_FIN) != 0) as u32;
        if flags & TCP_ACK != 0 {
            raw_tcp_send(src_ip, src_port, dst_port, ack, 0, TCP_RST, 0, &[]);
        } else {
            raw_tcp_send(src_ip, src_port, dst_port, 0, seq.wrapping_add(span), TCP_RST | TCP_ACK, 0, &[]);
        }
        return;
    }

    // A SYN to a listening socket -> spawn an accepted socket, SYN-ACK it.
    if flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {
        let listening = unsafe {
            (0..MAX_SOCKS)
                .any(|i| SOCKS[i].state == SockState::Listen && SOCKS[i].local_port == dst_port)
        } && bind_allows(dst_port, dst_ip);
        if listening {
            if let Some(n) = sock_alloc() {
                let isn = new_isn();
                unsafe {
                    let s = &mut SOCKS[n];
                    s.state = SockState::SynRcvd;
                    s.remote_ip = src_ip;
                    s.remote_port = src_port;
                    s.local_port = dst_port;
                    s.rcv_nxt = seq.wrapping_add(1);
                    s.snd_nxt = isn;
                    s.snd_una = isn;
                    s.snd_wnd = win as u32;
                    if let Some(m) = syn_opts.0 {
                        s.snd_mss = m.clamp(256, MSS as u16);
                    }
                }
                sock_send(n, TCP_SYN | TCP_ACK, &[]);
                txq_push(n, isn, TCP_SYN | TCP_ACK, &[]);
                unsafe { SOCKS[n].snd_nxt = SOCKS[n].snd_nxt.wrapping_add(1) };
            } else {
                // no socket free: refuse cleanly instead of leaving it hanging
                raw_tcp_send(src_ip, src_port, dst_port, 0, seq.wrapping_add(1), TCP_RST | TCP_ACK, 0, &[]);
            }
        } else if to_us {
            raw_tcp_send(src_ip, src_port, dst_port, 0, seq.wrapping_add(1), TCP_RST | TCP_ACK, 0, &[]);
        }
    }
}

/// Is this IPv4 frame addressed to our own unicast address?
fn dst_ip_is_us(buf: &[u8]) -> bool {
    buf.len() >= 34 && buf[30..34] == self_ip() && self_ip() != [0, 0, 0, 0]
}

/// Drain all currently-available NIC packets, routing each to its socket.
/// Bounded per call so a busy link can't starve the caller's own progress.
/// One service pass over the NIC (ARP, inbound TCP/UDP) for callers that are
/// not inside a socket wait -- programs polling `net_discover` / `net_status`.
pub fn pump_once() {
    let _io = IoGuard::new();
    pump();
}

fn pump() {
    let mut buf = [0u8; 2048];
    let mut n = 0;
    while let Some(len) = nic::receive(&mut buf) {
        unsafe { RX_FRAMES = RX_FRAMES.wrapping_add(1) };
        route_packet(&buf[..len]);
        n += 1;
        if n >= 64 {
            break;
        }
    }
    // what this machine sent to itself (127.0.0.0/8, its own address)
    let mut k = 0;
    while k < LOOP_SLOTS * 2 {
        let Some(len) = loop_pop(&mut buf) else { break };
        unsafe { FROM_LOOP = true };
        route_packet(&buf[..len]);
        unsafe { FROM_LOOP = false };
        k += 1;
    }
    tcp_timers();
}

// -- Loopback + bound listeners ------------------------------------------------
// TCP to 127.0.0.0/8 or to our own address goes round a small ring instead of
// the NIC (`raw_tcp_send` -> `loop_push`, `pump` -> `loop_pop` -> `route_packet`),
// so a server here is reachable from this machine at 127.0.0.1 (and at its own
// LAN address) even with no network at all. A full ring drops the frame, as a
// wire would; the retransmission timer resends it.
const LOOP_SLOTS: usize = 64;
const LOOP_FRAME: usize = 14 + 20 + 28 + MSS;
static mut LOOP_BUF: [[u8; LOOP_FRAME]; LOOP_SLOTS] = [[0; LOOP_FRAME]; LOOP_SLOTS];
static mut LOOP_LEN: [usize; LOOP_SLOTS] = [0; LOOP_SLOTS];
static mut LOOP_HEAD: usize = 0;
static mut LOOP_COUNT: usize = 0;
/// `route_packet` is handling a frame from the loopback ring (not the wire).
static mut FROM_LOOP: bool = false;

/// An IPv4 frame from the wire that claims 127.0.0.0/8 (RFC 1122 "martian"):
/// dropped, or a forged one would get past a listener bound to loopback only.
fn martian(buf: &[u8]) -> bool {
    !unsafe { FROM_LOOP } && buf.len() >= 34 && buf[12] == 0x08 && buf[13] == 0x00 && (buf[26] == 127 || buf[30] == 127)
}

/// Frames to `ip` stay on this machine: 127.0.0.0/8, or our own (configured) address.
fn is_local_dst(ip: [u8; 4]) -> bool {
    ip[0] == 127 || (ip == self_ip() && ip != [0, 0, 0, 0])
}

fn loop_push(frame: &[u8]) -> bool {
    unsafe {
        if LOOP_COUNT == LOOP_SLOTS || frame.len() > LOOP_FRAME {
            TX_FAILS = TX_FAILS.wrapping_add(1);
            return false;
        }
        let k = (LOOP_HEAD + LOOP_COUNT) % LOOP_SLOTS;
        (&mut *&raw mut LOOP_BUF)[k][..frame.len()].copy_from_slice(frame);
        LOOP_LEN[k] = frame.len();
        LOOP_COUNT += 1;
    }
    true
}

fn loop_pop(out: &mut [u8]) -> Option<usize> {
    unsafe {
        if LOOP_COUNT == 0 {
            return None;
        }
        let k = LOOP_HEAD;
        let n = LOOP_LEN[k].min(out.len());
        out[..n].copy_from_slice(&(&*&raw const LOOP_BUF)[k][..n]);
        LOOP_HEAD = (LOOP_HEAD + 1) % LOOP_SLOTS;
        LOOP_COUNT -= 1;
        Some(n)
    }
}

/// Listening ports bound to one local address: (port, address). A port not in
/// the table takes connections to any address (the historical behaviour).
const BIND_SLOTS: usize = 8;
static mut LISTEN_BIND: [(u16, [u8; 4]); BIND_SLOTS] = [(0, [0; 4]); BIND_SLOTS];

/// Bind listener `port` to `ip` before (or while) it listens, as bind(2) would:
/// * `0.0.0.0` -- every address: the network and 127.0.0.1;
/// * `127.x.x.x` -- loopback only: programs on this machine, nothing from the network;
/// * any other address -- only connections addressed to it (our LAN address; while
///   the machine does not have that address, nothing connects).
///
/// There is one NIC active at a time (wired or Wi-Fi), so "this machine's
/// addresses" are 127.0.0.1 and `self_ip()`.
pub fn tcp_listen_bind(port: u16, ip: [u8; 4]) {
    unsafe {
        let t = &mut *&raw mut LISTEN_BIND;
        if let Some(e) = t.iter_mut().find(|e| e.0 == port) {
            e.1 = ip;
            return;
        }
        if let Some(e) = t.iter_mut().find(|e| e.0 == 0) {
            *e = (port, ip);
        }
    }
}

/// Forget `port`'s binding (call with `tcp_unlisten`).
pub fn tcp_unbind(port: u16) {
    unsafe {
        for e in (&mut *&raw mut LISTEN_BIND).iter_mut() {
            if e.0 == port {
                *e = (0, [0; 4]);
            }
        }
    }
}

/// May a SYN addressed to `dst` open a connection on listener `port`?
fn bind_allows(port: u16, dst: [u8; 4]) -> bool {
    let found = unsafe { (&*&raw const LISTEN_BIND).iter().find(|e| e.0 == port && port != 0).copied() };
    let Some((_, ip)) = found else { return true };
    if ip == [0, 0, 0, 0] {
        true
    } else if ip[0] == 127 {
        dst[0] == 127
    } else {
        dst == ip
    }
}

/// Copy up to `sink.len()` bytes of socket `handle`'s buffered receive data into
/// `sink`, shifting the remainder down. Returns bytes copied.
fn drain_rx(handle: usize, sink: &mut [u8]) -> usize {
    if handle >= MAX_SOCKS || sink.is_empty() {
        return 0;
    }
    unsafe {
        let s = &mut SOCKS[handle];
        let take = s.rx_len.min(sink.len());
        if take == 0 {
            return 0;
        }
        sink[..take].copy_from_slice(&s.rx[..take]);
        s.rx.copy_within(take..s.rx_len, 0);
        s.rx_len -= take;
        // the window had closed (or nearly): say it is open again, or the
        // sender waits for its persist timer (seconds per window)
        let free = SOCK_RX_CAP.saturating_sub(s.rx_len).min(RX_WINDOW as usize) as u32;
        if s.last_adv < (4 * MSS) as u32 && free >= (8 * MSS) as u32 && s.state == SockState::Established {
            sock_send(handle, TCP_ACK, &[]);
        }
        take
    }
}

// -- Handle API (servers: sshd, messenger) ------------------------------------

/// Passive open: ensure a listener on `port`, then wait up to `budget_us` for an
/// inbound connection to complete its handshake. Returns the accepted socket
/// handle, or None on timeout. The listener persists for the next accept.
pub fn tcp_listen_accept(port: u16, budget_us: u64) -> Option<usize> {
    {
        let _io = IoGuard::new();
        let have = unsafe {
            (0..MAX_SOCKS).any(|i| SOCKS[i].state == SockState::Listen && SOCKS[i].local_port == port)
        };
        if !have {
            let l = sock_alloc()?;
            unsafe {
                SOCKS[l].state = SockState::Listen;
                SOCKS[l].local_port = port;
            }
        }
    }
    let mut accepted = None;
    timer::poll_until(budget_us, || {
        let _io = IoGuard::new();
        pump();
        unsafe {
            for i in 0..MAX_SOCKS {
                if SOCKS[i].state == SockState::Established
                    && SOCKS[i].local_port == port
                    && !SOCKS[i].accepted
                {
                    // Hand it out exactly once: mark it so a later accept on the
                    // same port doesn't re-return this now-live socket and run a
                    // second handshake that would tear it down.
                    SOCKS[i].accepted = true;
                    accepted = Some(i);
                    return true;
                }
            }
        }
        false
    });
    accepted
}

/// Stop listening on `port` (connections already accepted stay).
pub fn tcp_unlisten(port: u16) {
    let _io = IoGuard::new();
    for i in 0..MAX_SOCKS {
        if unsafe { SOCKS[i].state == SockState::Listen && SOCKS[i].local_port == port } {
            sock_free(i);
        }
    }
}

/// Send application data on socket `handle` (one segment, <= MSS).
pub fn tcp_write_h(handle: usize, data: &[u8]) -> bool {
    if handle >= MAX_SOCKS || data.len() > MSS {
        return false;
    }
    // a peer that takes smaller segments than ours gets them cut to its size
    let smss = (unsafe { SOCKS[handle].snd_mss } as usize).max(64);
    if data.len() > smss {
        return data.chunks(smss).all(|c| tcp_write_h(handle, c));
    }
    // room in the retransmission queue, the peer's window and the congestion window first
    let fits = || unsafe {
        let s = &SOCKS[handle];
        let in_flight = s.snd_nxt.wrapping_sub(s.snd_una);
        txq_count(handle) < TXQ_N && (in_flight + data.len() as u32 <= send_room(s) || in_flight == 0)
    };
    if !fits() {
        // waits as long as the peer keeps acknowledging; gives up only after WAIT_BUDGET_US
        // without any progress (a slow server is not a dead one)
        let mut ok = false;
        let mut last_una = unsafe { SOCKS[handle].snd_una };
        let mut quiet_since = timer::now_us();
        loop {
            timer::poll_until(500_000, || {
                let _io = IoGuard::new();
                pump();
                if unsafe { SOCKS[handle].state } != SockState::Established {
                    return true;
                }
                ok = fits();
                ok
            });
            if ok || unsafe { SOCKS[handle].state } != SockState::Established {
                break;
            }
            let una = unsafe { SOCKS[handle].snd_una };
            let now = timer::now_us();
            if una != last_una {
                last_una = una;
                quiet_since = now;
            } else if now.wrapping_sub(quiet_since) >= WAIT_BUDGET_US {
                break;
            }
        }
        if !ok {
            // why a send gave up (Discord's upload to Google Cloud Storage stalled here at byte 0
            // with 16 bytes unacked): the peer's window, our queue, what is in flight
            unsafe {
                let s = &SOCKS[handle];
                crate::console_write(
                    alloc::format!(
                        "tcp: send gave up on socket {handle}: last segment from the peer {} ms ago ({} in all), frames received {}, NIC send failures {}; peer window {} B (shift {}), cwnd {} B, mss {}, in flight {} B, retransmit queue {}/{}, segment {} B, state {:?}\n",
                        timer::now_ms().wrapping_sub(s.last_rx_ms),
                        s.rx_segs,
                        RX_FRAMES,
                        TX_FAILS,
                        s.snd_wnd,
                        s.snd_shift,
                        s.cwnd,
                        s.snd_mss,
                        s.snd_nxt.wrapping_sub(s.snd_una),
                        txq_count(handle),
                        TXQ_N,
                        data.len(),
                        s.state as u8
                    )
                    .as_bytes(),
                );
            }
            return false;
        }
    }
    let _io = IoGuard::new();
    if unsafe { SOCKS[handle].state } != SockState::Established {
        return false;
    }
    let seq = unsafe { SOCKS[handle].snd_nxt };
    // the NIC's ring full (a burst outran it) is not a dead connection: the segment is queued
    // all the same and the retransmission timer sends it (it used to fail the whole write)
    let _ = sock_send(handle, TCP_ACK | TCP_PSH, data);
    txq_push(handle, seq, TCP_ACK | TCP_PSH, data);
    unsafe {
        SOCKS[handle].snd_nxt = SOCKS[handle].snd_nxt.wrapping_add(data.len() as u32);
    }
    true
}

/// Read whatever arrives next on socket `handle` (returns as soon as any data is
/// buffered, or the peer closes, or `budget_us` elapses). Pumps the shared RX
/// under a brief lock each poll, so other sockets keep flowing meanwhile.
pub fn tcp_read_h(handle: usize, sink: &mut [u8], budget_us: u64) -> usize {
    if handle >= MAX_SOCKS {
        return 0;
    }
    let mut got = 0usize;
    timer::poll_until(budget_us, || {
        {
            let _io = IoGuard::new();
            pump();
            got = drain_rx(handle, sink);
        }
        let closed =
            unsafe { SOCKS[handle].state == SockState::Closed && SOCKS[handle].rx_len == 0 };
        got > 0 || closed
    });
    got
}

/// Best-effort close: FIN the connection and free the socket.
pub fn tcp_close_h(handle: usize) {
    if handle >= MAX_SOCKS {
        return;
    }
    // Graceful: let what we sent be acknowledged (up to 2 s), then FIN and
    // keep resending it until acknowledged (up to 1.5 s) -- the last frames of
    // a session (an SSH channel close) must not be lost to an early free.
    if unsafe { SOCKS[handle].state } == SockState::Established {
        timer::poll_until(2_000_000, || {
            let _io = IoGuard::new();
            pump();
            txq_count(handle) == 0 || unsafe { SOCKS[handle].state } != SockState::Established
        });
        let fin_sent = {
            let _io = IoGuard::new();
            if unsafe { SOCKS[handle].state } == SockState::Established {
                let seq = unsafe { SOCKS[handle].snd_nxt };
                let ok = sock_send(handle, TCP_FIN | TCP_ACK, &[]);
                if ok {
                    txq_push(handle, seq, TCP_FIN | TCP_ACK, &[]);
                    unsafe { SOCKS[handle].snd_nxt = seq.wrapping_add(1) };
                }
                ok
            } else {
                false
            }
        };
        if fin_sent {
            timer::poll_until(1_500_000, || {
                let _io = IoGuard::new();
                pump();
                txq_count(handle) == 0
            });
        }
    }
    let _io = IoGuard::new();
    sock_free(handle);
}

pub fn tcp_established_h(handle: usize) -> bool {
    handle < MAX_SOCKS && unsafe { SOCKS[handle].state == SockState::Established }
}

// -- Non-blocking handle helpers (cooperative callers that wait by yielding
// their coroutine, e.g. the LWP-1 peer-to-peer transport in `lwp/`) ----------

/// Start an active open and return at once (poll `tcp_established_h` /
/// `tcp_closed_h`; `tcp_free_h` to give up).
pub fn tcp_connect_begin_h(ip: [u8; 4], port: u16) -> Option<usize> {
    let _io = IoGuard::new();
    let h = sock_alloc()?;
    unsafe {
        let s = &mut SOCKS[h];
        s.state = SockState::SynSent;
        s.remote_ip = ip;
        s.remote_port = port;
        s.local_port = new_local_port();
        s.snd_nxt = new_isn();
        s.snd_una = s.snd_nxt;
        s.rcv_nxt = 0;
    }
    if !sock_send(h, TCP_SYN, &[]) {
        sock_free(h);
        return None;
    }
    unsafe {
        txq_push(h, SOCKS[h].snd_nxt, TCP_SYN, &[]);
        SOCKS[h].snd_nxt = SOCKS[h].snd_nxt.wrapping_add(1);
    }
    Some(h)
}

/// Debug: a socket's state, ports and sequence numbers.
pub fn tcp_debug_h(handle: usize) -> alloc::string::String {
    if handle >= MAX_SOCKS {
        return alloc::string::String::from("bad handle");
    }
    let s = unsafe { &SOCKS[handle] };
    let st = match s.state {
        SockState::Free => "free",
        SockState::Listen => "listen",
        SockState::SynSent => "syn-sent",
        SockState::SynRcvd => "syn-rcvd",
        SockState::Established => "established",
        SockState::Closed => "closed",
    };
    alloc::format!("{st} lport={} rport={} gw_mac={}", s.local_port, s.remote_port, unsafe { GW_MAC_KNOWN })
}

/// The connection is over (reset or closed by the peer, or the handle free).
pub fn tcp_closed_h(handle: usize) -> bool {
    handle >= MAX_SOCKS || unsafe { matches!(SOCKS[handle].state, SockState::Closed | SockState::Free) }
}

/// The remote end (ip, port) of a connection.
pub fn tcp_remote_h(handle: usize) -> Option<([u8; 4], u16)> {
    if handle >= MAX_SOCKS || unsafe { SOCKS[handle].state == SockState::Free } {
        return None;
    }
    unsafe { Some((SOCKS[handle].remote_ip, SOCKS[handle].remote_port)) }
}

/// Would `tcp_write_h(handle, <len bytes>)` go out now, without waiting for
/// the peer's window or the retransmission queue?
pub fn tcp_writable_h(handle: usize, len: usize) -> bool {
    if handle >= MAX_SOCKS || len > MSS {
        return false;
    }
    unsafe {
        let s = &SOCKS[handle];
        let in_flight = s.snd_nxt.wrapping_sub(s.snd_una);
        s.state == SockState::Established
            && txq_count(handle) < TXQ_N
            && (in_flight + len.min(s.snd_mss as usize) as u32 <= send_room(s) || in_flight == 0)
    }
}

/// Segments sent on `handle` and not yet acknowledged.
pub fn tcp_unacked_h(handle: usize) -> usize {
    if handle >= MAX_SOCKS {
        return 0;
    }
    txq_count(handle)
}

/// Send our FIN without waiting (pair with `tcp_free_h` once acknowledged).
pub fn tcp_fin_h(handle: usize) -> bool {
    let _io = IoGuard::new();
    if handle >= MAX_SOCKS || unsafe { SOCKS[handle].state } != SockState::Established {
        return false;
    }
    let seq = unsafe { SOCKS[handle].snd_nxt };
    let ok = sock_send(handle, TCP_FIN | TCP_ACK, &[]);
    if ok {
        txq_push(handle, seq, TCP_FIN | TCP_ACK, &[]);
        unsafe { SOCKS[handle].snd_nxt = seq.wrapping_add(1) };
    }
    ok
}

/// Free the socket now (no waiting; the peer sees a reset on its next segment).
pub fn tcp_free_h(handle: usize) {
    let _io = IoGuard::new();
    sock_free(handle);
}

/// Is a socket listening on `port`?
pub fn tcp_listening(port: u16) -> bool {
    unsafe { (0..MAX_SOCKS).any(|i| SOCKS[i].state == SockState::Listen && SOCKS[i].local_port == port) }
}

/// Active open on a dedicated socket (its own handle, distinct from the reserved
/// legacy client socket) so a caller like Messenger can dial a peer while TLS
/// and sshd use their own sockets. Returns the connected handle, or None.
pub fn tcp_connect_h(ip: [u8; 4], port: u16, budget_us: u64) -> Option<usize> {
    let h = {
        let _io = IoGuard::new();
        let h = sock_alloc()?;
        unsafe {
            let s = &mut SOCKS[h];
            s.state = SockState::SynSent;
            s.remote_ip = ip;
            s.remote_port = port;
            s.local_port = new_local_port();
            s.snd_nxt = new_isn();
            s.snd_una = s.snd_nxt;
            s.rcv_nxt = 0;
        }
        if !sock_send(h, TCP_SYN, &[]) {
            sock_free(h);
            return None;
        }
        unsafe {
            txq_push(h, SOCKS[h].snd_nxt, TCP_SYN, &[]);
            SOCKS[h].snd_nxt = SOCKS[h].snd_nxt.wrapping_add(1);
        }
        h
    };
    let mut ok = false;
    timer::poll_until(budget_us, || {
        let _io = IoGuard::new();
        pump();
        match unsafe { SOCKS[h].state } {
            SockState::Established => {
                ok = true;
                true
            },
            SockState::Closed => true,
            _ => false,
        }
    });
    if ok {
        Some(h)
    } else {
        tcp_close_h(h);
        None
    }
}

// -- Legacy client API (TLS/browser/lingfu, one connection at a time) ---------

/// Open a client connection to `ip:port` (three-way handshake), reusing a single
/// reserved client socket. A still-open previous client connection is dropped.
pub fn tcp_connect(ip: [u8; 4], port: u16) -> bool {
    let h = {
        let _net = NetGuard::new();
        unsafe {
            if CLIENT_HANDLE != usize::MAX {
                sock_free(CLIENT_HANDLE);
                CLIENT_HANDLE = usize::MAX;
            }
        }
        let Some(h) = sock_alloc() else { return false };
        unsafe {
            let s = &mut SOCKS[h];
            s.state = SockState::SynSent;
            s.remote_ip = ip;
            s.remote_port = port;
            s.local_port = new_local_port();
            s.snd_nxt = new_isn();
            s.snd_una = s.snd_nxt;
            s.rcv_nxt = 0;
            CLIENT_HANDLE = h;
        }
        if !sock_send(h, TCP_SYN, &[]) {
            return false;
        }
        unsafe {
            txq_push(h, SOCKS[h].snd_nxt, TCP_SYN, &[]);
            SOCKS[h].snd_nxt = SOCKS[h].snd_nxt.wrapping_add(1);
        }
        h
    };
    let mut ok = false;
    timer::poll_until(WAIT_BUDGET_US, || {
        let _net = NetGuard::new();
        pump();
        match unsafe { SOCKS[h].state } {
            SockState::Established => {
                ok = true;
                true
            },
            SockState::Closed => true,
            _ => false,
        }
    });
    ok
}

pub fn tcp_write(data: &[u8]) -> bool {
    tcp_write_h(unsafe { CLIENT_HANDLE }, data)
}

pub fn tcp_read_some(sink: &mut [u8], budget_us: u64) -> usize {
    tcp_read_h(unsafe { CLIENT_HANDLE }, sink, budget_us)
}

/// Read the client stream until the peer FINs, the sink is full, or the budget
/// expires -- accumulating across pumps.
pub fn tcp_read_to_end(sink: &mut [u8]) -> usize {
    let h = unsafe { CLIENT_HANDLE };
    if h >= MAX_SOCKS {
        return 0;
    }
    let mut got = 0usize;
    timer::poll_until(WAIT_BUDGET_US, || {
        {
            let _net = NetGuard::new();
            pump();
            if got < sink.len() {
                got += drain_rx(h, &mut sink[got..]);
            }
        }
        let done = unsafe { SOCKS[h].state == SockState::Closed && SOCKS[h].rx_len == 0 };
        got >= sink.len() || done
    });
    got
}

pub fn tcp_close() {
    let h = unsafe { CLIENT_HANDLE };
    tcp_close_h(h);
    unsafe { CLIENT_HANDLE = usize::MAX };
}

/// Is the client connection currently established?
pub fn tcp_is_established() -> bool {
    let h = unsafe { CLIENT_HANDLE };
    h != usize::MAX && tcp_established_h(h)
}

// -- UDP + DNS ---------------------------------------------------------------

/// Send one UDP datagram (gateway-routed, like everything here).
/// Shared UDP/IPv4/Ethernet frame builder. `udp_send` (gateway-routed),
/// `udp_broadcast`, and `udp_send_to` (direct LAN peer) all funnel through
/// this -- only the destination MAC and destination IP differ.
fn build_and_send_udp(dst_mac: [u8; 6], dst_ip: [u8; 4], dst_port: u16, src_port: u16, payload: &[u8]) -> bool {
    build_and_send_udp_ttl(dst_mac, dst_ip, dst_port, src_port, payload, 64)
}

fn build_and_send_udp_ttl(dst_mac: [u8; 6], dst_ip: [u8; 4], dst_port: u16, src_port: u16, payload: &[u8], ttl: u8) -> bool {
    let _io = IoGuard::new();
    if payload.len() > 512 {
        return false;
    }
    let udp_len = 8 + payload.len();
    let ip_len = 20 + udp_len;
    let mut f = [0u8; 14 + 20 + 8 + 512];
    f[0..6].copy_from_slice(&dst_mac);
    f[6..12].copy_from_slice(&nic::mac());
    f[12..14].copy_from_slice(&[0x08, 0x00]);
    let ip = &mut f[14..];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
    ip[6] = 0x40;
    ip[8] = ttl;
    ip[9] = 17; // UDP
    ip[12..16].copy_from_slice(&self_ip());
    ip[16..20].copy_from_slice(&dst_ip);
    let ipsum = checksum(&ip[..20], 0);
    ip[10..12].copy_from_slice(&ipsum.to_be_bytes());
    let u = &mut ip[20..];
    u[0..2].copy_from_slice(&src_port.to_be_bytes());
    u[2..4].copy_from_slice(&dst_port.to_be_bytes());
    u[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    // UDP checksum optional over IPv4: zero = none. SLIRP accepts that.
    u[8..8 + payload.len()].copy_from_slice(payload);
    nic::transmit(&f[..(14 + ip_len).max(60)])
}

fn udp_send(dst_ip: [u8; 4], dst_port: u16, src_port: u16, payload: &[u8]) -> bool {
    let Some(gw) = arp_gateway() else { return false };
    build_and_send_udp(gw, dst_ip, dst_port, src_port, payload)
}

/// Send a UDP datagram to the local broadcast address (255.255.255.255) --
/// used for LAN peer discovery (see `drivers/messenger.rs`). The Ethernet
/// destination is the broadcast MAC, so every host on the LAN segment
/// receives it regardless of ARP state, unlike `udp_send` which always
/// routes via the gateway's MAC (wrong for reaching a same-subnet peer
/// directly, and useless for a destination with no single fixed address).
pub fn udp_broadcast(dst_port: u16, src_port: u16, payload: &[u8]) -> bool {
    if !nic::ensure_init() {
        return false;
    }
    build_and_send_udp([0xFF; 6], [255, 255, 255, 255], dst_port, src_port, payload)
}

/// Send a UDP datagram directly to a known LAN peer, ARP-resolving the
/// peer's own MAC (see `arp_resolve` -- `arp_gateway`'s cache is
/// gateway-only and can't be reused here).
pub fn udp_send_to(dst_ip: [u8; 4], dst_port: u16, src_port: u16, payload: &[u8]) -> bool {
    let Some(mac) = arp_resolve(dst_ip) else { return false };
    build_and_send_udp(mac, dst_ip, dst_port, src_port, payload)
}

/// Poll for a UDP datagram to `port`; payload into `out`.
fn udp_poll(port: u16, out: &mut [u8]) -> Option<usize> {
    udp_poll_from(port, out).map(|(_ip, n)| n)
}

/// Poll for a UDP datagram to `port` from any sender, also returning the
/// sender's IP -- needed to learn a discovered peer's address (LAN
/// discovery replies come from whoever answered a broadcast, not a
/// pre-known address) or to reply directly instead of re-broadcasting.
///
/// Shares the NIC's single receive queue with every other poller in this
/// driver (DNS, DHCP, TCP, ARP) -- same "one thing polls at a time"
/// contract the module doc already discloses for TCP; a caller waiting on
/// this should not be mid-poll on something else concurrently.
pub fn udp_poll_from(port: u16, out: &mut [u8]) -> Option<([u8; 4], usize)> {
    udp_want(port);
    let _io = IoGuard::new();
    if let Some(r) = udp_mail_take(port, out) {
        return Some(r);
    }
    let mut buf = [0u8; 2048];
    let len = nic::receive(&mut buf)?;
    // Is this the UDP datagram on `port` we're after?
    if len >= 42 && buf[12] == 0x08 && buf[13] == 0x00 && buf[23] == 17 {
        let ihl = ((buf[14] & 0x0F) as usize) * 4;
        let u = 14 + ihl;
        if u + 8 <= len && u16::from_be_bytes([buf[u + 2], buf[u + 3]]) == port {
            let mut src_ip = [0u8; 4];
            src_ip.copy_from_slice(&buf[26..30]);
            let ulen = u16::from_be_bytes([buf[u + 4], buf[u + 5]]) as usize;
            let n = ulen.saturating_sub(8).min(out.len()).min(len - u - 8);
            out[..n].copy_from_slice(&buf[u + 8..u + 8 + n]);
            return Some((src_ip, n));
        }
    }
    // Not our datagram -- but the desktop's per-frame beacon poll shares the one
    // NIC receive queue with the background chat task's pump. Dropping whatever
    // else arrives (ARP requests, TCP for open sockets/listeners) would starve
    // that task's connections. Route it through the central handler instead so
    // no frame is lost to whichever consumer happened to dequeue it.
    route_packet(&buf[..len]);
    None
}

// -- Generic ARP resolution -------------------------------------------------
// `arp_gateway` above only ever resolves the gateway. LAN peer traffic
// needs the peer's OWN MAC (sending an Ethernet frame addressed to the
// gateway with a same-subnet destination IP doesn't reliably reach a LAN
// peer). Small fixed-size cache: a messenger's peer count is inherently
// small (friends + nearby discovered hosts), so a handful of entries is
// enough, no eviction policy needed yet.
const ARP_CACHE_LEN: usize = 16;
static mut ARP_CACHE_IP: [[u8; 4]; ARP_CACHE_LEN] = [[0; 4]; ARP_CACHE_LEN];
static mut ARP_CACHE_MAC: [[u8; 6]; ARP_CACHE_LEN] = [[0; 6]; ARP_CACHE_LEN];
static mut ARP_CACHE_USED: [bool; ARP_CACHE_LEN] = [false; ARP_CACHE_LEN];

/// Resolve `ip`'s MAC via ARP (cached after the first success). Shorter
/// timeout than `arp_gateway`'s (which affords 8s for CI/TSC-calibration
/// edge cases on a link known to exist): a messenger resolving several
/// LAN peers interactively can't afford to block the desktop loop that
/// long per unresolved address, and healthy LAN ARP replies arrive in ms.
pub fn arp_resolve(ip: [u8; 4]) -> Option<[u8; 6]> {
    if !nic::ensure_init() {
        return None;
    }
    unsafe {
        for i in 0..ARP_CACHE_LEN {
            if ARP_CACHE_USED[i] && ARP_CACHE_IP[i] == ip {
                return Some(ARP_CACHE_MAC[i]);
            }
        }
    }
    let my_mac = nic::mac();
    let mut frame = [0u8; 60];
    frame[0..6].copy_from_slice(&[0xFF; 6]);
    frame[6..12].copy_from_slice(&my_mac);
    frame[12..14].copy_from_slice(&[0x08, 0x06]);
    frame[14..16].copy_from_slice(&[0x00, 0x01]);
    frame[16..18].copy_from_slice(&[0x08, 0x00]);
    frame[18] = 6;
    frame[19] = 4;
    frame[20..22].copy_from_slice(&[0x00, 0x01]);
    frame[22..28].copy_from_slice(&my_mac);
    frame[28..32].copy_from_slice(&self_ip());
    frame[32..38].copy_from_slice(&[0x00; 6]);
    frame[38..42].copy_from_slice(&ip);
    if !{
        let _io = IoGuard::new();
        nic::transmit(&frame)
    } {
        return None;
    }
    let mut buf = [0u8; 2048];
    let mut found = None;
    timer::poll_until(1_500_000, || {
        let _io = IoGuard::new();
        if unsafe { ARP_CACHE_USED.iter().zip(ARP_CACHE_IP.iter()).any(|(u, a)| *u && *a == ip) } {
            // another task's pump filed the reply
            found = unsafe { (0..ARP_CACHE_LEN).find(|&i| ARP_CACHE_USED[i] && ARP_CACHE_IP[i] == ip).map(|i| ARP_CACHE_MAC[i]) };
            return found.is_some();
        }
        if let Some(len) = nic::receive(&mut buf) {
            if !(len >= 42 && buf[12] == 0x08 && buf[13] == 0x06) {
                route_packet(&buf[..len]);
                return false;
            }
            if len >= 42 && buf[12] == 0x08 && buf[13] == 0x06 {
                if buf[20] == 0x00 && buf[21] == 0x02 && buf[28..32] == ip {
                    let mut reply_mac = [0u8; 6];
                    reply_mac.copy_from_slice(&buf[22..28]);
                    found = Some(reply_mac);
                    return true;
                }
                // Any other ARP frame (notably the peer's OWN request, when it
                // is dialing us at the same moment) -- answer + cache it, so a
                // simultaneous two-way connect doesn't deadlock with both sides
                // waiting for a reply neither is free to send.
                handle_arp(&buf[..len]);
            }
        }
        false
    });
    if let Some(mac) = found {
        arp_cache_put(ip, mac);
    }
    found
}

/// DNS servers, tried in order with failover. Primary is a LAN resolver
/// (the requested 192.168.0.2); secondary is Cloudflare 1.1.1.1 -- a
/// cutting-edge, privacy-first public resolver (not Google/Microsoft/
/// Yahoo), reachable through QEMU *and* VirtualBox NAT, so names resolve
/// even when the LAN resolver is absent. Both overridable at runtime via
/// `set_dns` (the desktop reads lingfs `/dns` at boot: two ip lines).
/// Insert or refresh an `ip -> mac` mapping in the ARP cache.
fn arp_cache_put(ip: [u8; 4], mac: [u8; 6]) {
    unsafe {
        for i in 0..ARP_CACHE_LEN {
            if ARP_CACHE_USED[i] && ARP_CACHE_IP[i] == ip {
                ARP_CACHE_MAC[i] = mac;
                return;
            }
        }
        for i in 0..ARP_CACHE_LEN {
            if !ARP_CACHE_USED[i] {
                ARP_CACHE_IP[i] = ip;
                ARP_CACHE_MAC[i] = mac;
                ARP_CACHE_USED[i] = true;
                return;
            }
        }
    }
}

/// Handle an inbound ARP frame: cache the sender's mapping, and if it's a
/// request for our own IP, transmit a reply. This is the passive counterpart
/// to `arp_resolve` -- required for a LAN peer to reach us, since on a bare L2
/// segment (e.g. two LingOS machines on a QEMU socket/mcast link) there's no
/// gateway or SLIRP to answer ARP for us.
fn handle_arp(buf: &[u8]) {
    // Ethernet+ARP header: htype(2) ptype(2) hlen plen oper(2) sha(6) spa(4)
    // tha(6) tpa(4), starting at byte 14. Require Ethernet/IPv4 ARP.
    if buf.len() < 42 || buf[14] != 0x00 || buf[15] != 0x01 || buf[16] != 0x08 || buf[17] != 0x00 {
        return;
    }
    let oper = u16::from_be_bytes([buf[20], buf[21]]);
    let mut sha = [0u8; 6];
    sha.copy_from_slice(&buf[22..28]);
    let mut spa = [0u8; 4];
    spa.copy_from_slice(&buf[28..32]);
    let mut tpa = [0u8; 4];
    tpa.copy_from_slice(&buf[38..42]);
    if spa != [0; 4] {
        arp_cache_put(spa, sha); // learn from requests and replies alike
    }
    if oper != 1 || tpa != self_ip() {
        return; // only reply to a request aimed at us
    }
    let my_mac = nic::mac();
    let mut f = [0u8; 42];
    f[0..6].copy_from_slice(&sha); // to the requester
    f[6..12].copy_from_slice(&my_mac);
    f[12..14].copy_from_slice(&[0x08, 0x06]);
    f[14..16].copy_from_slice(&[0x00, 0x01]); // htype Ethernet
    f[16..18].copy_from_slice(&[0x08, 0x00]); // ptype IPv4
    f[18] = 6;
    f[19] = 4;
    f[20..22].copy_from_slice(&[0x00, 0x02]); // oper = reply
    f[22..28].copy_from_slice(&my_mac);
    f[28..32].copy_from_slice(&self_ip());
    f[32..38].copy_from_slice(&sha);
    f[38..42].copy_from_slice(&spa);
    let _ = nic::transmit(&f);
}

// Public resolvers reachable from real hardware AND QEMU/VBox NAT (SLIRP
// forwards them). The old default primary was a LAN guess (192.168.0.2) that
// isn't reachable anywhere but that one network, so every first lookup ate a
// 1.5s timeout before failing over -- and it failed outright on real metal.
pub const PRIMARY_DNS: [u8; 4] = [1, 1, 1, 1]; // Cloudflare
pub const SECONDARY_DNS: [u8; 4] = [8, 8, 8, 8]; // Google
static mut DNS1: [u8; 4] = PRIMARY_DNS;
static mut DNS2: [u8; 4] = SECONDARY_DNS;

pub fn set_dns(primary: [u8; 4], secondary: [u8; 4]) {
    unsafe {
        DNS1 = primary;
        DNS2 = secondary;
    }
}

pub fn dns_servers() -> ([u8; 4], [u8; 4]) {
    unsafe { (DNS1, DNS2) }
}

/// A short budget per DNS server so failover is snappy on VirtualBox,
/// where the LAN primary (192.168.0.2) isn't on the NAT and would
/// otherwise stall every lookup before Cloudflare answers.
const DNS_BUDGET_US: u64 = 1_500_000;

/// One DNS A-query to a specific server. Returns the first A record, or
/// None on timeout / NXDOMAIN.
fn dns_query(server: [u8; 4], host: &str) -> Option<[u8; 4]> {
    let mut q = [0u8; 300];
    q[0] = b'L';
    q[1] = b'Q';
    q[2] = 0x01; // RD
    q[5] = 1; // QDCOUNT
    let mut n = 12;
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 || n + label.len() + 1 > 280 {
            return None;
        }
        q[n] = label.len() as u8;
        n += 1;
        q[n..n + label.len()].copy_from_slice(label.as_bytes());
        n += label.len();
    }
    q[n] = 0;
    n += 1;
    q[n..n + 4].copy_from_slice(&[0, 1, 0, 1]); // QTYPE A, QCLASS IN
    n += 4;

    let src_port = 33000 + (timer::now_ms() % 8000) as u16;
    if !udp_send(server, 53, src_port, &q[..n]) {
        return None;
    }
    let mut resp = [0u8; 512];
    let mut found: Option<[u8; 4]> = None;
    timer::poll_until(DNS_BUDGET_US, || {
        let Some(rlen) = udp_poll(src_port, &mut resp) else { return false };
        if rlen < 12 || resp[0] != b'L' || resp[1] != b'Q' {
            return false;
        }
        let ancount = u16::from_be_bytes([resp[6], resp[7]]) as usize;
        if ancount == 0 {
            return true; // authoritative "no such name" -- stop waiting
        }
        let mut p = 12;
        while p < rlen && resp[p] != 0 {
            p += resp[p] as usize + 1;
        }
        p += 5; // 0 terminator + QTYPE/QCLASS
        for _ in 0..ancount {
            if p + 12 > rlen {
                return true;
            }
            if resp[p] & 0xC0 == 0xC0 {
                p += 2;
            } else {
                while p < rlen && resp[p] != 0 {
                    p += resp[p] as usize + 1;
                }
                p += 1;
            }
            let rtype = u16::from_be_bytes([resp[p], resp[p + 1]]);
            let rdlen = u16::from_be_bytes([resp[p + 8], resp[p + 9]]) as usize;
            p += 10;
            if rtype == 1 && rdlen == 4 && p + 4 <= rlen {
                found = Some([resp[p], resp[p + 1], resp[p + 2], resp[p + 3]]);
                return true;
            }
            p += rdlen;
        }
        true
    });
    found
}

/// The NAT built-in resolver (QEMU SLIRP *and* VirtualBox NAT both expose
/// their DNS proxy here). Used as a guaranteed last-resort fallback: a LAN
/// resolver like 192.168.0.2 or an external one like 1.1.1.1 isn't always
/// reachable from inside a NAT, but 10.0.2.3 always is -- so resolution
/// works out of the box on both hypervisors while still honoring the
/// configured servers first on real hardware/LANs.
pub const NAT_RESOLVER: [u8; 4] = [10, 0, 2, 3];

static mut DNS_OWNER: i64 = -1;
struct DnsGuard;
impl DnsGuard {
    fn new() -> Self {
        let me = crate::coro::task_key();
        unsafe {
            while DNS_OWNER != -1 && DNS_OWNER != me {
                net_spin();
            }
            DNS_OWNER = me;
        }
        DnsGuard
    }
}
impl Drop for DnsGuard {
    fn drop(&mut self) {
        unsafe { DNS_OWNER = -1 };
    }
}

/// Resolve `host` to an IPv4 address: dotted-quad short-circuits; otherwise
/// a real DNS A query -- configured primary, then secondary, then the NAT
/// resolver as a guaranteed fallback.
pub fn dns_resolve(host: &str) -> Option<[u8; 4]> {
    if let Some(ip) = parse_ipv4(host) {
        return Some(ip);
    }
    if let Some(ip) = dns_cache_get(host) {
        return Some(ip);
    }
    // A shared claim (DNS runs inside every fetch) and one lookup at a time;
    // the answer lands in a UDP mailbox, whoever pumps the NIC.
    let _net = NetShare::new();
    let _dns = DnsGuard::new();
    if let Some(ip) = dns_cache_get(host) {
        return Some(ip); // another task looked it up while we waited
    }
    // Resolve the gateway MAC ONCE. Every server below routes through the
    // same gateway, so if the gateway is unreachable there's no point paying
    // the ARP timeout three times over -- bail immediately. `arp_gateway`
    // caches the failure, so this returns in microseconds on an offline box
    // (the old path froze the desktop for ~24s: 3 servers x 8s ARP each).
    if arp_gateway().is_none() {
        return None;
    }
    let (d1, d2) = unsafe { (DNS1, DNS2) };
    // The configured servers, then the gateway (most home routers resolve),
    // then public resolvers: a wrong DNS entry must not take the network down.
    let mut tried: [[u8; 4]; 6] = [[0; 4]; 6];
    let mut n = 0;
    for server in [d1, d2, gateway_ip(), [1, 1, 1, 1], [8, 8, 8, 8], NAT_RESOLVER] {
        if server == [0, 0, 0, 0] || tried[..n].contains(&server) {
            continue;
        }
        tried[n] = server;
        n += 1;
        if let Some(ip) = dns_query(server, host) {
            return Some(ip);
        }
    }
    None
}

/// One query to one server (the connection test names who answered).
pub fn dns_query_at(server: [u8; 4], host: &str) -> Option<[u8; 4]> {
    let _net = NetGuard::new();
    if arp_gateway().is_none() && !same_subnet(server) {
        return None;
    }
    dns_query(server, host)
}

/// The time from an SNTP server (RFC 4330), as unix seconds: a few public
/// servers in turn, the first answer wins. National time institutes and pools
/// outside the US: Thailand's NIMT, Alibaba Cloud (China), Switzerland's
/// METAS, then the Thai and Asian pools.
pub fn sntp_time() -> Option<u64> {
    sntp_time_ms().map(|ms| ms / 1000)
}

/// The network time in milliseconds since 1970 (the server's transmit time, fraction included:
/// the clock measures its own rate from two of these, and whole seconds were too coarse).
pub fn sntp_time_ms() -> Option<u64> {
    const NTP_TO_UNIX: u64 = 2_208_988_800;
    for host in ["time1.nimt.or.th", "ntp.aliyun.com", "ntp.metas.ch", "th.pool.ntp.org", "asia.pool.ntp.org", "pool.ntp.org", "europe.pool.ntp.org"] {
        let Some(server) = dns_resolve(host) else { continue };
        let _net = NetGuard::new();
        let mut q = [0u8; 48];
        q[0] = 0x1B; // LI 0, version 3, mode 3 (client)
        let src_port = 41000 + (timer::now_ms() % 8000) as u16;
        if !udp_send(server, 123, src_port, &q) {
            continue;
        }
        let mut resp = [0u8; 64];
        let mut got: Option<u64> = None;
        let sent_us = timer::now_us();
        timer::poll_until(DNS_BUDGET_US, || {
            let Some(n) = udp_poll(src_port, &mut resp) else { return false };
            if n < 48 || resp[0] & 0x07 != 4 {
                return false; // not a server reply
            }
            let secs = u32::from_be_bytes([resp[40], resp[41], resp[42], resp[43]]) as u64;
            let frac = u32::from_be_bytes([resp[44], resp[45], resp[46], resp[47]]) as u64;
            if secs > NTP_TO_UNIX {
                // the server stamped its reply about half a round trip ago
                let half_rtt_ms = timer::now_us().saturating_sub(sent_us) / 2000;
                got = Some((secs - NTP_TO_UNIX) * 1000 + ((frac * 1000) >> 32) + half_rtt_ms);
            }
            true
        });
        if got.is_some() {
            return got;
        }
    }
    None
}

// -- DHCP client -------------------------------------------------------------

/// What we parse out of a DHCP OFFER/ACK. Any field left zero means the
/// server didn't send that option.
#[derive(Clone, Copy)]
struct DhcpReply {
    yiaddr: [u8; 4],
    mask: [u8; 4],
    router: [u8; 4],
    dns: [u8; 4],
    server_id: [u8; 4],
    msg_type: u8,
    /// Option 51: the lease length in seconds (0 = not sent).
    lease: u32,
}
impl DhcpReply {
    const EMPTY: Self = Self {
        yiaddr: [0; 4],
        mask: [0; 4],
        router: [0; 4],
        dns: [0; 4],
        server_id: [0; 4],
        msg_type: 0,
        lease: 0,
    };
}

// The lease we hold (DHCP only; a static address never expires): when it was acquired, how long it
// lasts, and when the next renewal attempt is due. Without renewing, the router may hand the address
// to another device once the lease runs out and every connection of this machine drops.
static mut LEASE_AT_MS: u64 = 0;
static mut LEASE_SECS: u32 = 0;
static mut LEASE_IP: [u8; 4] = [0; 4];
static mut LEASE_NEXT_MS: u64 = 0;

fn lease_note(ip: [u8; 4], secs: u32) {
    unsafe {
        LEASE_AT_MS = timer::now_ms();
        LEASE_SECS = secs;
        LEASE_IP = ip;
        LEASE_NEXT_MS = 0;
    }
}

/// From `startup::tick` (about once a second while a program presents frames): renew the DHCP lease.
/// At half the lease time ask the server to confirm our address (a short, bounded exchange, retried
/// every 30 s); if it refuses, or the lease is nearly over with no answer, start over with a full
/// DHCP exchange. Costs one comparison per call until then.
pub fn lease_tick(now: u64) {
    let (at, secs, ip, next) = unsafe { (LEASE_AT_MS, LEASE_SECS, LEASE_IP, LEASE_NEXT_MS) };
    if secs == 0 || ip == [0; 4] || now < next {
        return;
    }
    let age = now.saturating_sub(at);
    let life = secs as u64 * 1000;
    if age < life / 2 {
        return;
    }
    if unsafe { (*(&raw const STATIC_CFG)).is_some() } {
        return; // a static address was set in Settings: nothing to renew
    }
    unsafe { LEASE_NEXT_MS = now + 30_000 };
    let mac = nic::mac();
    let xid = (now as u32) ^ ((mac[5] as u32) << 8) ^ 0x4C65_6173;
    // INIT-REBOOT form: the address we hold in option 50, no server id; any server that knows the
    // lease answers with an ACK (same address, a fresh lease) or a NAK (it is not ours any more)
    let mut req = [0u8; 16];
    req[0..3].copy_from_slice(&[53, 1, 3]);
    req[3..5].copy_from_slice(&[50, 4]);
    req[5..9].copy_from_slice(&ip);
    req[9..14].copy_from_slice(&[55, 3, 1, 3, 6]);
    req[14] = 255;
    let mut got = DhcpReply::EMPTY;
    let answered = dhcp_send(mac, xid, &req[..15]) && dhcp_await(xid, 0, &mut got, 120_000) && (got.msg_type == 5 || got.msg_type == 6);
    if answered && got.msg_type == 5 && got.yiaddr == ip {
        lease_note(ip, if got.lease > 0 { got.lease } else { secs });
        crate::console_write(b"net: DHCP lease renewed\n");
        return;
    }
    let nak = answered && got.msg_type == 6;
    if nak || age + 60_000 >= life {
        crate::console_write(if nak { b"net: DHCP server refused the lease; asking again\n" } else { b"net: DHCP lease about to end; asking again\n" });
        if !dhcp_configure() {
            unsafe { LEASE_NEXT_MS = now + 30_000 };
        }
    }
}

/// Build and broadcast one BOOTREQUEST (op=1) with the given options. Uses
/// raw frames (not `udp_send`) because DHCP happens before we have an IP or
/// a known gateway: Ethernet dst = broadcast, IP src 0.0.0.0 -> 255.255.
/// 255.255, UDP 68 -> 67, and the BOOTP broadcast flag set so the server
/// broadcasts its reply (we can't receive a unicast to an IP we don't hold
/// yet).
fn dhcp_send(mac: [u8; 6], xid: u32, opts: &[u8]) -> bool {
    let dhcp_len = 240 + opts.len();
    let udp_len = 8 + dhcp_len;
    let ip_len = 20 + udp_len;
    let total = 14 + ip_len;
    let mut f = [0u8; 14 + 20 + 8 + 240 + 40];
    if total > f.len() {
        return false;
    }
    f[0..6].copy_from_slice(&[0xFF; 6]);
    f[6..12].copy_from_slice(&mac);
    f[12..14].copy_from_slice(&[0x08, 0x00]);
    let ip = &mut f[14..];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
    ip[6] = 0x40;
    ip[8] = 64;
    ip[9] = 17; // UDP
    ip[12..16].copy_from_slice(&[0, 0, 0, 0]);
    ip[16..20].copy_from_slice(&[255, 255, 255, 255]);
    let ipsum = checksum(&ip[..20], 0);
    ip[10..12].copy_from_slice(&ipsum.to_be_bytes());
    let u = &mut ip[20..];
    u[0..2].copy_from_slice(&68u16.to_be_bytes());
    u[2..4].copy_from_slice(&67u16.to_be_bytes());
    u[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    // UDP checksum optional over IPv4 -> left zero (SLIRP and real servers
    // accept it), same as udp_send.
    let d = &mut u[8..];
    d[0] = 1; // op: BOOTREQUEST
    d[1] = 1; // htype: Ethernet
    d[2] = 6; // hlen
    d[4..8].copy_from_slice(&xid.to_be_bytes());
    d[10..12].copy_from_slice(&0x8000u16.to_be_bytes()); // flags: broadcast
    d[28..34].copy_from_slice(&mac); // chaddr
    d[236..240].copy_from_slice(&[99, 130, 83, 99]); // DHCP magic cookie
    d[240..240 + opts.len()].copy_from_slice(opts);
    let ok = nic::transmit(&f[..total.max(60)]);
    if !ok {
        unsafe { TX_FAILS = TX_FAILS.wrapping_add(1) };
    }
    ok
}

/// Poll (bounded) for a broadcast BOOTREPLY matching `xid` and, if given, a
/// specific DHCP message type (2=OFFER, 5=ACK). Fills `out` and returns true
/// on a match.
fn dhcp_await(xid: u32, want_type: u8, out: &mut DhcpReply, budget_us: u64) -> bool {
    let mut buf = [0u8; 2048];
    let mut done = false;
    timer::poll_until(budget_us, || {
        let Some(len) = nic::receive(&mut buf) else { return false };
        if len < 14 + 20 + 8 + 240 || buf[12] != 0x08 || buf[13] != 0x00 || buf[23] != 17 {
            return false;
        }
        let ihl = ((buf[14] & 0x0F) as usize) * 4;
        let u = 14 + ihl;
        if u + 8 > len || u16::from_be_bytes([buf[u + 2], buf[u + 3]]) != 68 {
            return false; // not to our DHCP client port
        }
        let d = u + 8;
        if d + 240 > len
            || buf[d] != 2 // BOOTREPLY
            || buf[d + 4..d + 8] != xid.to_be_bytes()
            || buf[d + 236..d + 240] != [99, 130, 83, 99]
        {
            return false;
        }
        let mut r = DhcpReply::EMPTY;
        r.yiaddr.copy_from_slice(&buf[d + 16..d + 20]);
        let mut p = d + 240;
        while p + 1 < len {
            let code = buf[p];
            if code == 255 {
                break;
            }
            if code == 0 {
                p += 1;
                continue;
            }
            let l = buf[p + 1] as usize;
            if p + 2 + l > len {
                break;
            }
            let v = &buf[p + 2..p + 2 + l];
            match code {
                53 if l >= 1 => r.msg_type = v[0],
                1 if l >= 4 => r.mask.copy_from_slice(&v[..4]),
                3 if l >= 4 => r.router.copy_from_slice(&v[..4]),
                6 if l >= 4 => r.dns.copy_from_slice(&v[..4]),
                54 if l >= 4 => r.server_id.copy_from_slice(&v[..4]),
                51 if l >= 4 => r.lease = u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
                _ => {},
            }
            p += 2 + l;
        }
        if want_type != 0 && r.msg_type != want_type {
            return false;
        }
        *out = r;
        done = true;
        true
    });
    done
}

/// Configure our IPv4 via DHCP (DISCOVER -> OFFER -> REQUEST -> ACK). This is
/// what makes DNS work on a VirtualBox *bridged*/*host-only* adapter: the
/// lease's IP/mask/router/DNS come from the LAN's real server, so ARP finds
/// the real gateway and off-subnet lookups (1.1.1.1, or the LAN resolver in
/// option 6) actually route -- instead of the wrong 10.0.2.x SLIRP defaults.
///
/// When no DHCP server answered, derive a static IPv4 from the MAC so two
/// LingOS machines on the *same* DHCP-less L2 segment don't both sit on the
/// SLIRP default `10.0.2.15`. This is exactly the QEMU `-netdev socket`
/// (or `mcast`) peer-to-peer setup used to test two instances against each
/// other: there's no DHCP there, but each guest has a distinct MAC. Keeps the
/// `10.0.2.0/24` net and `.2` gateway convention; the last octet is
/// `10 + (mac[5] & 0x7f)`, so it stays clear of `.0/.1/.2/.255` and differs
/// whenever the low MAC bytes differ. A real network's DHCP lease overrides
/// all of this (this only runs when `dhcp_configure` returned false).
pub fn apply_static_ip_from_mac() {
    let mac = nic::mac();
    let last = 10u8.wrapping_add(mac[5] & 0x7f);
    nic::set_ip_config([10, 0, 2, last], [255, 255, 255, 0], [10, 0, 2, 2]);
    invalidate_gateway();
}

/// A user-set static IPv4 configuration (address, mask, gateway, DNS) --
/// Settings > Network on the desktop. While one is set, every way of getting
/// an address (`configure`, `dhcp_configure`: boot, Wi-Fi joins, the apps)
/// applies it instead of asking DHCP. Kept in lingfs `netstatic` so it holds
/// from boot on, before anyone has signed in.
static mut STATIC_CFG: Option<([u8; 4], [u8; 4], [u8; 4], [u8; 4])> = None;
static mut STATIC_LOADED: bool = false;
const STATIC_FILE: &str = "netstatic";

pub fn set_static(cfg: Option<([u8; 4], [u8; 4], [u8; 4], [u8; 4])>) {
    unsafe {
        *(&raw mut STATIC_CFG) = cfg;
        STATIC_LOADED = true;
    }
    if crate::fs::lingfs::is_mounted() {
        let text = match cfg {
            Some((ip, mask, gw, dns)) => {
                let f = |a: [u8; 4]| alloc::format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
                alloc::format!("static {} {} {} {}\n", f(ip), f(mask), f(gw), f(dns))
            },
            None => alloc::string::String::from("dhcp\n"),
        };
        let _ = crate::fs::lingfs::write_file_any(STATIC_FILE, text.as_bytes());
    }
    apply_static();
}

pub fn static_config() -> Option<([u8; 4], [u8; 4], [u8; 4], [u8; 4])> {
    // the saved setting, read the first time storage is there
    unsafe {
        if !STATIC_LOADED && crate::fs::lingfs::is_mounted() {
            STATIC_LOADED = true;
            let mut buf = [0u8; 4096];
            if let Ok(Some(n)) = crate::fs::lingfs::read_file(STATIC_FILE, &mut buf) {
                let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
                let mut it = text.split_whitespace();
                if it.next() == Some("static") {
                    let ips: alloc::vec::Vec<[u8; 4]> = it.filter_map(parse_ip4).collect();
                    if ips.len() >= 3 {
                        let dns = ips.get(3).copied().unwrap_or(ips[2]);
                        *(&raw mut STATIC_CFG) = Some((ips[0], ips[1], ips[2], dns));
                    }
                }
            }
        }
        *(&raw const STATIC_CFG)
    }
}

fn parse_ip4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for part in s.split('.') {
        if n == 4 {
            return None;
        }
        out[n] = part.parse().ok()?;
        n += 1;
    }
    if n == 4 { Some(out) } else { None }
}

/// Put the static configuration on the NIC, if there is one.
pub fn apply_static() -> bool {
    let Some((ip, mask, gw, dns)) = static_config() else { return false };
    if !nic::ensure_init() {
        return false;
    }
    nic::set_ip_config(ip, mask, gw);
    set_dns(dns, [8, 8, 8, 8]);
    invalidate_gateway();
    true
}

/// Get an address: the static one when set, DHCP otherwise.
pub fn configure() -> bool {
    crate::hitch::timed(b"network setup (DHCP)", configure_untimed)
}

fn configure_untimed() -> bool {
    if apply_static() {
        crate::clock::sync_from_network();
        return true;
    }
    dhcp_configure()
}

/// Best-effort and bounded: on a network with no DHCP server it returns
/// false after the timeouts and the static SLIRP fallback stays in place, so
/// plain QEMU and manual static-IP setups still work. Runs once at boot.
pub fn dhcp_configure() -> bool {
    // a static address set in Settings > Network wins over DHCP everywhere
    if apply_static() {
        crate::clock::sync_from_network();
        return true;
    }
    if !nic::ensure_init() {
        return false;
    }
    let mac = nic::mac();
    let xid = (timer::now_ms() as u32) ^ ((mac[4] as u32) << 8) ^ (mac[5] as u32) ^ 0x4C69_6E00;

    let mut tries = 0;
    while tries < 2 {
        tries += 1;
        let disc: &[u8] = &[53, 1, 1, 55, 4, 1, 3, 6, 15, 255];
        if !dhcp_send(mac, xid, disc) {
            return false;
        }
        let mut offer = DhcpReply::EMPTY;
        if !dhcp_await(xid, 2, &mut offer, 2_000_000) || offer.yiaddr == [0; 4] {
            continue;
        }
        // REQUEST the offered address (SELECTING state: ciaddr stays 0, the
        // wanted IP goes in option 50, and we echo the server id in 54).
        let mut req = [0u8; 32];
        let mut n = 0;
        req[n..n + 3].copy_from_slice(&[53, 1, 3]);
        n += 3;
        req[n..n + 2].copy_from_slice(&[50, 4]);
        n += 2;
        req[n..n + 4].copy_from_slice(&offer.yiaddr);
        n += 4;
        req[n..n + 2].copy_from_slice(&[54, 4]);
        n += 2;
        req[n..n + 4].copy_from_slice(&offer.server_id);
        n += 4;
        req[n..n + 5].copy_from_slice(&[55, 3, 1, 3, 6]);
        n += 5;
        req[n] = 255;
        n += 1;
        if !dhcp_send(mac, xid, &req[..n]) {
            return false;
        }
        // Some servers' ACK omits fields already sent in the OFFER; fall back
        // to the OFFER's values when the ACK is silent (or never arrives).
        let mut ack = DhcpReply::EMPTY;
        if !dhcp_await(xid, 5, &mut ack, 2_000_000) {
            ack = offer;
        }
        let ip = if ack.yiaddr != [0; 4] { ack.yiaddr } else { offer.yiaddr };
        let gw = if ack.router != [0; 4] { ack.router } else { offer.router };
        if ip == [0; 4] || gw == [0; 4] {
            continue; // unusable lease -- keep the static fallback
        }
        let mask = if ack.mask != [0; 4] {
            ack.mask
        } else if offer.mask != [0; 4] {
            offer.mask
        } else {
            [255, 255, 255, 0]
        };
        nic::set_ip_config(ip, mask, gw);
        invalidate_gateway();
        // remember the lease so it can be renewed (an ACK that omits the time falls back to the OFFER's)
        lease_note(ip, if ack.lease > 0 { ack.lease } else { offer.lease });
        // Prefer the lease's DNS as primary, keep Cloudflare as secondary.
        let dns = if ack.dns != [0; 4] { ack.dns } else { offer.dns };
        if dns != [0; 4] {
            set_dns(dns, SECONDARY_DNS);
        }
        // the Pi 5's clock restarts at 1970 without a battery: ask the network
        crate::clock::sync_from_network();
        return true;
    }
    false
}

fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut part = 0;
    let mut acc: u32 = 0;
    let mut digits = 0;
    for b in s.bytes() {
        match b {
            b'0'..=b'9' => {
                acc = acc * 10 + (b - b'0') as u32;
                digits += 1;
                if acc > 255 || digits > 3 {
                    return None;
                }
            },
            b'.' => {
                if digits == 0 || part >= 3 {
                    return None;
                }
                ip[part] = acc as u8;
                part += 1;
                acc = 0;
                digits = 0;
            },
            _ => return None,
        }
    }
    if part == 3 && digits > 0 {
        ip[3] = acc as u8;
        Some(ip)
    } else {
        None
    }
}

/// Parse `http://host[:port]/path` (https is recognized and REFUSED with
/// None-plus-reason -- no TLS stack yet, said out loud rather than
/// silently downgraded). Returns (host, port, path-start-index, is_tls).
pub fn parse_url(url: &str) -> Option<(&str, u16, &str, bool)> {
    let (rest, tls) = if let Some(r) = url.strip_prefix("http://") {
        (r, false)
    } else if let Some(r) = url.strip_prefix("https://") {
        (r, true)
    } else {
        (url, false)
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.find(':') {
        Some(i) => {
            let mut p: u32 = 0;
            for b in hostport[i + 1..].bytes() {
                if !b.is_ascii_digit() {
                    return None;
                }
                p = p * 10 + (b - b'0') as u32;
                if p > 65535 {
                    return None;
                }
            }
            (&hostport[..i], p as u16)
        },
        // No explicit port: default by scheme (443 for https, 80 for http).
        // Returning the scheme default here -- rather than a fixed 80 -- means
        // every caller connects to the right port even if it forgets to
        // special-case the TLS default. (A TLS handshake against port 80 gets
        // a plaintext HTTP 400 back, which the record parser then chokes on.)
        None => (hostport, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, path, tls))
}

/// HTTP/1.0 GET `path` from `ip:port`, returning the FULL raw response
/// (status line + headers + body, whatever the status code) into `out`.
/// Unlike `http_get`, this doesn't filter on status 200 or strip headers --
/// callers that need to see a redirect (a `Location` header on a 3xx) use
/// this directly and parse the header block themselves (see
/// `drivers::horizon`'s `fetch_raw`). `http_get` below stays the simple
/// 200-only/body-only convenience wrapper existing callers already rely on.
pub fn http_get_raw(ip: [u8; 4], port: u16, path: &str, host: &str, out: &mut [u8]) -> Option<usize> {
    let _net = NetGuard::new();
    if !tcp_connect(ip, port) {
        return None;
    }
    let mut req = [0u8; 512];
    let mut n = 0;
    for part in [b"GET " as &[u8], path.as_bytes(), b" HTTP/1.0\r\nHost: ", host.as_bytes(), b"\r\nUser-Agent: lingfu/1 (LingOS)\r\n\r\n"] {
        if n + part.len() > req.len() {
            return None;
        }
        req[n..n + part.len()].copy_from_slice(part);
        n += part.len();
    }
    if !tcp_write(&req[..n]) {
        return None;
    }
    static mut RAW_RESP: [u8; 128 * 1024] = [0; 128 * 1024];
    let resp = unsafe { &mut *&raw mut RAW_RESP };
    let total = tcp_read_to_end(resp);
    if total < 12 || &resp[..7] != b"HTTP/1." {
        return None;
    }
    let len = total.min(out.len());
    out[..len].copy_from_slice(&resp[..len]);
    Some(len)
}

/// HTTP/1.0 GET `path` from `ip:port`. Returns the body length written to
/// `body` (headers parsed and stripped), or None on connect/protocol
/// failure. HTTP/1.0 keeps it simple: no chunked encoding, connection
/// closes at end-of-body -- exactly the framing `tcp_read_to_end` gives.
pub fn http_get(ip: [u8; 4], port: u16, path: &str, host: &str, body: &mut [u8]) -> Option<usize> {
    let _net = NetGuard::new();
    if !tcp_connect(ip, port) {
        return None;
    }
    let mut req = [0u8; 512];
    let mut n = 0;
    for part in [b"GET " as &[u8], path.as_bytes(), b" HTTP/1.0\r\nHost: ", host.as_bytes(), b"\r\nUser-Agent: lingfu/1 (LingOS)\r\n\r\n"] {
        if n + part.len() > req.len() {
            return None;
        }
        req[n..n + part.len()].copy_from_slice(part);
        n += part.len();
    }
    if !tcp_write(&req[..n]) {
        return None;
    }
    // Response into a static-side scratch first (headers + body).
    static mut RESP: [u8; 128 * 1024] = [0; 128 * 1024];
    let resp = unsafe { &mut *&raw mut RESP };
    let total = tcp_read_to_end(resp);
    if total < 12 || &resp[..7] != b"HTTP/1." {
        return None;
    }
    // Status code.
    let code = (resp[9] - b'0') as u32 * 100 + (resp[10] - b'0') as u32 * 10 + (resp[11] - b'0') as u32;
    if code != 200 {
        return None;
    }
    // Find the blank line ending the headers.
    let mut body_start = 0;
    for i in 0..total.saturating_sub(3) {
        if &resp[i..i + 4] == b"\r\n\r\n" {
            body_start = i + 4;
            break;
        }
    }
    if body_start == 0 {
        return None;
    }
    let len = (total - body_start).min(body.len());
    body[..len].copy_from_slice(&resp[body_start..body_start + len]);
    Some(len)
}
