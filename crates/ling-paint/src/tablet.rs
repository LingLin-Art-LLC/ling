//! Pen tablets on Windows, two ways:
//! * Windows Ink (WM_POINTER, Windows 8+): every pen with a Windows Ink driver - a Wacom Cintiq / Intuos with
//!   "Use Windows Ink" on (the default), Surface pens, Huion / XP-Pen. The window is subclassed by
//!   [`Tablet::attach`] and the pen's coalesced samples (GetPointerPenInfoHistory: every report, not one per
//!   frame) are collected with pressure, tilt, rotation, the barrel button and the eraser end. Positions are the
//!   same screen pixels the cursor uses, so a pen display that is a second monitor lands on the right spot.
//! * WinTab (wintab32.dll, shipped by Wacom, Huion, XP-Pen, Gaomon drivers), opened on request with
//!   [`Tablet::open`]: pressure, tilt (altitude / azimuth), barrel twist and the eraser end, at the pen's packet
//!   rate, mapped over the area the driver's default system context covers.
//! While Windows Ink delivers, WinTab packets (the same pen, twice) are dropped. Positions come back in screen
//! pixels with sub-pixel precision; the host maps them into its window. Without either, painting falls back to
//! the mouse with simulated pressure.

#[derive(Clone, Copy, Debug, Default)]
pub struct Packet {
    pub sx: f32,
    pub sy: f32,
    pub p: f32,
    /// 0 upright .. 1 flat
    pub tilt: f32,
    /// radians, clockwise from up
    pub az: f32,
    pub twist: f32,
    pub eraser: bool,
    /// bit 0 tip, bit 1 barrel button, bit 2 second barrel button
    pub buttons: u32,
    /// the tip / eraser touches the surface (false while hovering in range)
    pub contact: bool,
}

#[cfg(all(windows, feature = "std"))]
mod imp {
    use super::Packet;
    use libloading::{Library, Symbol};

    const WTI_DEFSYSCTX: u32 = 4;
    const WTI_DEVICES: u32 = 100;
    const WTI_INTERFACE: u32 = 1;
    const IFC_WINTABID: u32 = 1;
    const DVC_NAME: u32 = 1;
    const DVC_NPRESSURE: u32 = 15;
    const DVC_ORIENTATION: u32 = 17;
    const PK_STATUS: u32 = 0x0002;
    const PK_CURSOR: u32 = 0x0020;
    const PK_BUTTONS: u32 = 0x0040;
    const PK_X: u32 = 0x0080;
    const PK_Y: u32 = 0x0100;
    const PK_NORMAL_PRESSURE: u32 = 0x0400;
    const PK_ORIENTATION: u32 = 0x1000;
    const CXO_SYSTEM: u32 = 0x0001;
    const OVERSAMPLE: i32 = 8;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct LogContextA {
        lc_name: [u8; 40],
        lc_options: u32,
        lc_status: u32,
        lc_locks: u32,
        lc_msg_base: u32,
        lc_device: u32,
        lc_pkt_rate: u32,
        lc_pkt_data: u32,
        lc_pkt_mode: u32,
        lc_move_mask: u32,
        lc_btn_dn_mask: u32,
        lc_btn_up_mask: u32,
        lc_in_org_x: i32,
        lc_in_org_y: i32,
        lc_in_org_z: i32,
        lc_in_ext_x: i32,
        lc_in_ext_y: i32,
        lc_in_ext_z: i32,
        lc_out_org_x: i32,
        lc_out_org_y: i32,
        lc_out_org_z: i32,
        lc_out_ext_x: i32,
        lc_out_ext_y: i32,
        lc_out_ext_z: i32,
        lc_sens_x: u32,
        lc_sens_y: u32,
        lc_sens_z: u32,
        lc_sys_mode: i32,
        lc_sys_org_x: i32,
        lc_sys_org_y: i32,
        lc_sys_ext_x: i32,
        lc_sys_ext_y: i32,
        lc_sys_sens_x: u32,
        lc_sys_sens_y: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Axis {
        min: i32,
        max: i32,
        units: u32,
        resolution: u32,
    }
    /// field order follows the bit order of the packet mask
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Pkt {
        status: u32,
        cursor: u32,
        buttons: u32,
        x: i32,
        y: i32,
        pressure: u32,
        azimuth: i32,
        altitude: i32,
        twist: i32,
    }

    type WtInfo = unsafe extern "system" fn(u32, u32, *mut u8) -> u32;
    type WtOpen = unsafe extern "system" fn(isize, *mut LogContextA, i32) -> isize;
    type WtClose = unsafe extern "system" fn(isize) -> i32;
    type WtPacketsGet = unsafe extern "system" fn(isize, i32, *mut Pkt) -> i32;
    type WtQueueSizeSet = unsafe extern "system" fn(isize, i32) -> i32;
    type WtOverlap = unsafe extern "system" fn(isize, i32) -> i32;

    pub struct Tablet {
        lib: Option<Library>,
        ctx: isize,
        pmax: f32,
        alt_max: f32,
        org: (i32, i32),
        pub name: String,
        /// when Windows Ink last delivered a pen sample
        ink_at: Option<std::time::Instant>,
    }
    impl Default for Tablet {
        fn default() -> Self {
            Tablet { lib: None, ctx: 0, pmax: 1023.0, alt_max: 900.0, org: (0, 0), name: String::new(), ink_at: None }
        }
    }
    impl Tablet {
        pub fn is_open(&self) -> bool {
            self.ctx != 0 || self.ink_at.is_some()
        }
        /// Read Windows Ink pen input arriving at `hwnd` (idempotent; a new window is subclassed again).
        pub fn attach(&mut self, hwnd: isize) {
            super::ink::attach(hwnd);
        }
        pub fn open(&mut self, hwnd: isize) -> Result<String, String> {
            if self.ctx != 0 {
                return Ok(self.name.clone());
            }
            unsafe {
                let lib = Library::new("wintab32.dll").map_err(|_| "no WinTab driver (wintab32.dll) - install your tablet driver, or keep using the mouse with simulated pressure".to_string())?;
                let info: Symbol<WtInfo> = lib.get(b"WTInfoA\0").map_err(|e| e.to_string())?;
                let open: Symbol<WtOpen> = lib.get(b"WTOpenA\0").map_err(|e| e.to_string())?;
                if info(0, 0, std::ptr::null_mut()) == 0 {
                    return Err("WinTab reports no tablet services".into());
                }
                let mut lc: LogContextA = std::mem::zeroed();
                if info(WTI_DEFSYSCTX, 0, &mut lc as *mut _ as *mut u8) == 0 {
                    return Err("WinTab has no default context".into());
                }
                let mut ax = Axis::default();
                if info(WTI_DEVICES, DVC_NPRESSURE, &mut ax as *mut _ as *mut u8) > 0 && ax.max > 0 {
                    self.pmax = ax.max as f32;
                }
                let mut ori = [Axis::default(); 3];
                if info(WTI_DEVICES, DVC_ORIENTATION, ori.as_mut_ptr() as *mut u8) > 0 && ori[1].max > 0 {
                    self.alt_max = ori[1].max as f32;
                }
                let mut nm = [0u8; 128];
                info(WTI_DEVICES, DVC_NAME, nm.as_mut_ptr());
                let mut dev = String::from_utf8_lossy(&nm).trim_end_matches('\0').trim().to_string();
                if dev.is_empty() {
                    info(WTI_INTERFACE, IFC_WINTABID, nm.as_mut_ptr());
                    dev = String::from_utf8_lossy(&nm).trim_end_matches('\0').trim().to_string();
                }
                let name = b"LingAnime\0";
                lc.lc_name = [0; 40];
                lc.lc_name[..name.len()].copy_from_slice(name);
                lc.lc_options |= CXO_SYSTEM;
                let mask = PK_STATUS | PK_CURSOR | PK_BUTTONS | PK_X | PK_Y | PK_NORMAL_PRESSURE | PK_ORIENTATION;
                lc.lc_pkt_data = mask;
                lc.lc_pkt_mode = 0;
                lc.lc_move_mask = mask;
                lc.lc_btn_up_mask = lc.lc_btn_dn_mask;
                // output in (oversampled) screen pixels over the area the driver maps the tablet to, y down
                self.org = (lc.lc_sys_org_x, lc.lc_sys_org_y);
                lc.lc_out_org_x = 0;
                lc.lc_out_org_y = 0;
                lc.lc_out_ext_x = lc.lc_sys_ext_x * OVERSAMPLE;
                lc.lc_out_ext_y = -(lc.lc_sys_ext_y * OVERSAMPLE);
                let ctx = open(hwnd, &mut lc, 1);
                if ctx == 0 {
                    return Err("WTOpen failed".into());
                }
                if let Ok(q) = lib.get::<WtQueueSizeSet>(b"WTQueueSizeSet\0") {
                    let mut n = 256;
                    while n >= 16 && q(ctx, n) == 0 {
                        n /= 2;
                    }
                }
                if let Ok(ov) = lib.get::<WtOverlap>(b"WTOverlap\0") {
                    ov(ctx, 1);
                }
                self.ctx = ctx;
                self.name = if dev.is_empty() { "WinTab tablet".into() } else { dev };
                self.lib = Some(lib);
            }
            Ok(self.name.clone())
        }
        pub fn close(&mut self) {
            if let (Some(lib), true) = (&self.lib, self.ctx != 0) {
                unsafe {
                    if let Ok(c) = lib.get::<WtClose>(b"WTClose\0") {
                        c(self.ctx);
                    }
                }
            }
            self.ctx = 0;
            self.lib = None;
        }
        pub fn poll(&mut self) -> Vec<Packet> {
            let ink = super::ink::take();
            if !ink.is_empty() {
                if self.ink_at.is_none() && self.ctx == 0 {
                    self.name = "Windows Ink pen".into();
                }
                self.ink_at = Some(std::time::Instant::now());
            }
            let wt = self.poll_wintab();
            // the same pen reaches both APIs: Windows Ink wins while it is delivering
            if self.ink_at.is_some_and(|t| t.elapsed().as_secs_f32() < 1.0) {
                return ink;
            }
            wt
        }
        fn poll_wintab(&mut self) -> Vec<Packet> {
            let mut out = Vec::new();
            let Some(lib) = &self.lib else { return out };
            if self.ctx == 0 {
                return out;
            }
            unsafe {
                let Ok(get) = lib.get::<WtPacketsGet>(b"WTPacketsGet\0") else { return out };
                let mut buf = [Pkt::default(); 128];
                loop {
                    let n = get(self.ctx, buf.len() as i32, buf.as_mut_ptr());
                    if n <= 0 {
                        break;
                    }
                    for p in &buf[..n as usize] {
                        let alt = (p.altitude.abs() as f32 / self.alt_max).clamp(0.0, 1.0);
                        out.push(Packet {
                            sx: self.org.0 as f32 + p.x as f32 / OVERSAMPLE as f32,
                            sy: self.org.1 as f32 + p.y as f32 / OVERSAMPLE as f32,
                            p: (p.pressure as f32 / self.pmax).clamp(0.0, 1.0),
                            tilt: 1.0 - alt,
                            az: (p.azimuth as f32 / 10.0).to_radians(),
                            twist: (p.twist as f32 / 10.0).to_radians(),
                            eraser: p.cursor % 3 == 2 || p.altitude < 0,
                            buttons: p.buttons,
                            contact: p.pressure > 0 || p.buttons & 1 != 0,
                        });
                    }
                    if (n as usize) < buf.len() {
                        break;
                    }
                }
            }
            out
        }
    }
    impl Drop for Tablet {
        fn drop(&mut self) {
            self.close();
        }
    }
}

#[cfg(not(all(windows, feature = "std")))]
mod imp {
    use super::Packet;
    #[allow(unused_imports)]
    use crate::prelude::*;
    #[derive(Default)]
    pub struct Tablet {
        pub name: String,
    }
    impl Tablet {
        pub fn is_open(&self) -> bool {
            false
        }
        pub fn open(&mut self, _hwnd: isize) -> Result<String, String> {
            Err("pen tablets are read through WinTab on Windows; elsewhere the mouse with simulated pressure is used".into())
        }
        pub fn close(&mut self) {}
        pub fn attach(&mut self, _hwnd: isize) {}
        pub fn poll(&mut self) -> Vec<Packet> {
            Vec::new()
        }
    }
}

/// Windows Ink: the host window is subclassed and WM_POINTER pen messages are turned into [`Packet`]s.
#[cfg(all(windows, feature = "std"))]
mod ink {
    use super::Packet;
    use libloading::Library;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    const WM_POINTERUPDATE: u32 = 0x0245;
    const WM_POINTERLEAVE: u32 = 0x024A;
    const PT_PEN: u32 = 3;
    const POINTER_FLAG_INCONTACT: u32 = 0x4;
    const PEN_FLAG_BARREL: u32 = 0x1;
    const PEN_FLAG_INVERTED: u32 = 0x2;
    const PEN_FLAG_ERASER: u32 = 0x4;
    const PEN_MASK_PRESSURE: u32 = 0x1;
    const PEN_MASK_ROTATION: u32 = 0x2;
    const PEN_MASK_TILT_X: u32 = 0x4;
    const PEN_MASK_TILT_Y: u32 = 0x8;
    const GWLP_WNDPROC: i32 = -4;
    /// no press-and-hold ring / right-click, no tap / barrel feedback, no flicks while drawing
    const TABLET_FLAGS: isize = 0x1 | 0x8 | 0x10 | 0x10000;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Pt {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        l: i32,
        t: i32,
        r: i32,
        b: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PointerInfo {
        pointer_type: u32,
        pointer_id: u32,
        frame_id: u32,
        pointer_flags: u32,
        source_device: isize,
        hwnd_target: isize,
        pt_pixel: Pt,
        pt_himetric: Pt,
        pt_pixel_raw: Pt,
        pt_himetric_raw: Pt,
        time: u32,
        history_count: u32,
        input_data: i32,
        key_states: u32,
        performance_count: u64,
        button_change: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PenInfo {
        info: PointerInfo,
        pen_flags: u32,
        pen_mask: u32,
        pressure: u32,
        rotation: u32,
        tilt_x: i32,
        tilt_y: i32,
    }

    type GetPointerType = unsafe extern "system" fn(u32, *mut u32) -> i32;
    type GetPointerPenInfo = unsafe extern "system" fn(u32, *mut PenInfo) -> i32;
    type GetPointerPenInfoHistory = unsafe extern "system" fn(u32, *mut u32, *mut PenInfo) -> i32;
    type GetPointerDeviceRects = unsafe extern "system" fn(isize, *mut Rect, *mut Rect) -> i32;

    struct Api {
        _lib: Library,
        ptype: GetPointerType,
        pen: GetPointerPenInfo,
        history: GetPointerPenInfoHistory,
        rects: Option<GetPointerDeviceRects>,
    }
    // the function pointers are plain user32 entry points
    unsafe impl Send for Api {}
    unsafe impl Sync for Api {}

    #[link(name = "user32")]
    extern "system" {
        fn SetWindowLongPtrW(hwnd: isize, idx: i32, v: isize) -> isize;
        fn CallWindowProcW(prev: isize, hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize;
        fn SetPropW(hwnd: isize, name: *const u16, data: isize) -> i32;
    }

    static API: OnceLock<Option<Api>> = OnceLock::new();
    static HWND: AtomicIsize = AtomicIsize::new(0);
    static OLD_PROC: AtomicIsize = AtomicIsize::new(0);
    static QUEUE: Mutex<Vec<Packet>> = Mutex::new(Vec::new());

    fn api() -> Option<&'static Api> {
        API.get_or_init(|| unsafe {
            // Windows 8+ (user32 exports the pointer API there); looked up at run time so older systems still start
            let lib = Library::new("user32.dll").ok()?;
            let ptype = *lib.get::<GetPointerType>(b"GetPointerType\0").ok()?;
            let pen = *lib.get::<GetPointerPenInfo>(b"GetPointerPenInfo\0").ok()?;
            let history = *lib.get::<GetPointerPenInfoHistory>(b"GetPointerPenInfoHistory\0").ok()?;
            let rects = lib.get::<GetPointerDeviceRects>(b"GetPointerDeviceRects\0").ok().map(|f| *f);
            Some(Api { _lib: lib, ptype, pen, history, rects })
        })
        .as_ref()
    }

    pub fn attach(hwnd: isize) {
        if hwnd == 0 || HWND.load(Ordering::Relaxed) == hwnd || api().is_none() {
            return;
        }
        unsafe {
            let name: Vec<u16> = "MicrosoftTabletPenServiceProperty\0".encode_utf16().collect();
            SetPropW(hwnd, name.as_ptr(), TABLET_FLAGS);
            let old = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as isize);
            if old != 0 {
                OLD_PROC.store(old, Ordering::Relaxed);
                HWND.store(hwnd, Ordering::Relaxed);
            }
        }
    }

    pub fn take() -> Vec<Packet> {
        QUEUE.lock().map(|mut q| core::mem::take(&mut *q)).unwrap_or_default()
    }

    unsafe extern "system" fn wndproc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize {
        if (WM_POINTERUPDATE..=WM_POINTERLEAVE).contains(&msg) {
            collect(msg, (wp & 0xFFFF) as u32);
        }
        // the window's own procedure (minifb's) still sees everything: DefWindowProc turns the pen into the
        // mouse messages mouse_x / mouse_down read
        CallWindowProcW(OLD_PROC.load(Ordering::Relaxed), hwnd, msg, wp, lp)
    }

    unsafe fn collect(msg: u32, id: u32) {
        let Some(a) = api() else { return };
        let mut t = 0u32;
        if (a.ptype)(id, &mut t) == 0 || t != PT_PEN {
            return;
        }
        let mut buf = [PenInfo::default(); 64];
        let mut n = 0usize;
        if msg == WM_POINTERUPDATE {
            let mut cnt = buf.len() as u32;
            if (a.history)(id, &mut cnt, buf.as_mut_ptr()) != 0 {
                n = (cnt as usize).min(buf.len());
            }
        }
        if n == 0 {
            if (a.pen)(id, &mut buf[0]) == 0 {
                return;
            }
            n = 1;
        }
        let Ok(mut q) = QUEUE.lock() else { return };
        // the history comes newest first
        for pi in buf[..n].iter().rev() {
            q.push(to_packet(a, pi));
        }
        if q.len() > 4096 {
            let k = q.len() - 4096;
            q.drain(..k);
        }
    }

    unsafe fn to_packet(a: &Api, pi: &PenInfo) -> Packet {
        let i = &pi.info;
        let contact = i.pointer_flags & POINTER_FLAG_INCONTACT != 0;
        // sub-pixel position from the HIMETRIC location and the device's rectangles; the whole-pixel location
        // when they disagree (a DPI-virtualized window sees scaled pixels)
        let (mut sx, mut sy) = (i.pt_pixel.x as f32, i.pt_pixel.y as f32);
        if let Some(rects) = a.rects {
            let (mut dev, mut disp) = (Rect::default(), Rect::default());
            if rects(i.source_device, &mut dev, &mut disp) != 0 && dev.r > dev.l && dev.b > dev.t {
                let fx = disp.l as f32 + (i.pt_himetric.x - dev.l) as f32 * (disp.r - disp.l) as f32 / (dev.r - dev.l) as f32;
                let fy = disp.t as f32 + (i.pt_himetric.y - dev.t) as f32 * (disp.b - disp.t) as f32 / (dev.b - dev.t) as f32;
                if (fx - sx).abs() < 1.5 && (fy - sy).abs() < 1.5 {
                    (sx, sy) = (fx, fy);
                }
            }
        }
        let p = if pi.pen_mask & PEN_MASK_PRESSURE != 0 { (pi.pressure as f32 / 1024.0).clamp(0.0, 1.0) } else if contact { 1.0 } else { 0.0 };
        // tilt: the angles (degrees) the pen leans along screen x / y -> how far from upright (0..1, 1 = flat)
        // and the azimuth, clockwise from up (the WinTab convention the brushes use)
        let (tx, ty) = (
            if pi.pen_mask & PEN_MASK_TILT_X != 0 { pi.tilt_x as f32 } else { 0.0 },
            if pi.pen_mask & PEN_MASK_TILT_Y != 0 { pi.tilt_y as f32 } else { 0.0 },
        );
        let (gx, gy) = (tx.clamp(-89.0, 89.0).to_radians().tan(), ty.clamp(-89.0, 89.0).to_radians().tan());
        let lean = (gx * gx + gy * gy).sqrt().atan();
        let tilt = (lean / core::f32::consts::FRAC_PI_2).clamp(0.0, 1.0);
        let az = if lean > 0.001 { gx.atan2(-gy) } else { 0.0 };
        let twist = if pi.pen_mask & PEN_MASK_ROTATION != 0 { (pi.rotation as f32).to_radians() } else { 0.0 };
        let barrel = pi.pen_flags & PEN_FLAG_BARREL != 0;
        Packet {
            sx,
            sy,
            p: if contact { p } else { 0.0 },
            tilt,
            az,
            twist,
            eraser: pi.pen_flags & (PEN_FLAG_INVERTED | PEN_FLAG_ERASER) != 0,
            buttons: (contact as u32) | ((barrel as u32) << 1),
            contact,
        }
    }
}

pub use imp::Tablet;
