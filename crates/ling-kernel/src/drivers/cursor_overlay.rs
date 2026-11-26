//! The pointer as an overlay drawn straight into video memory (x86, 32 bpp).
//!
//! Before this, the pointer was painted into each app frame (`lingapp::decorate`)
//! and only moved when the foreground app presented: an app drawing at 20 fps had
//! a 20 fps pointer, and any long frame (a GC pause, the DAW's audio refill, a
//! big 3-D scene) was a visible hitch. Now the mouse interrupt itself moves the
//! pointer: it puts back the pixels it covered (the save-under) and draws it at
//! the new spot, at the mouse's own report rate, whatever the app is doing.
//!
//! Present still owns the screen: `begin_blit` lifts the pointer off and blocks
//! the interrupt from drawing while the frame is copied (the copy runs over
//! several cores), `end_blit` puts it back on the fresh frame. Anything else that
//! writes the whole screen calls `forget` (the frame overwrote the pointer).
//! The interrupt-side draw is ~176 pixel reads + writes: a few microseconds.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::{cursor, framebuffer};

const N: usize = (cursor::W * cursor::H) as usize;

/// Present has turned the overlay on for the app on screen (off: the app draws
/// its own pointer, a screenshot is being taken, or a non-32-bpp panel).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// A frame is being copied to the screen: hands off.
static BLITTING: AtomicBool = AtomicBool::new(false);
static mut DRAWN: Option<(i32, i32)> = None;
static mut UNDER: [u32; N] = [0; N];

#[inline]
fn irq_off<R>(f: impl FnOnce() -> R) -> R {
    let flags = unsafe { crate::arch::cpu::read_flags() };
    unsafe { crate::arch::cpu::cli() };
    let r = f();
    unsafe { crate::arch::cpu::write_flags(flags) };
    r
}

/// Whether this display can carry the overlay (32-bpp linear framebuffer).
pub fn supported() -> bool {
    framebuffer::get().map(|f| f.bpp == 32).unwrap_or(false)
}

/// Put back the pixels under the pointer. IRQs must be off.
unsafe fn restore() {
    let Some((x, y)) = DRAWN.take() else { return };
    let Some(fb) = framebuffer::get() else { return };
    let under = &*(&raw const UNDER);
    for dy in 0..cursor::H {
        let py = y + dy;
        if py < 0 || py >= fb.height as i32 {
            continue;
        }
        let row = (fb.addr as usize + py as usize * fb.pitch as usize) as *mut u32;
        for dx in 0..cursor::W {
            let px = x + dx;
            if px < 0 || px >= fb.width as i32 || cursor::classify(dx, dy) == 0 {
                continue;
            }
            core::ptr::write_volatile(row.add(px as usize), under[(dy * cursor::W + dx) as usize]);
        }
    }
}

/// Save what is under (x, y) and draw the pointer there. IRQs must be off.
unsafe fn draw(x: i32, y: i32) {
    let Some(fb) = framebuffer::get() else { return };
    if fb.bpp != 32 {
        return;
    }
    let fill = cursor::fill_color(crate::arch::timer::now_ms());
    let under = &mut *(&raw mut UNDER);
    for dy in 0..cursor::H {
        let py = y + dy;
        if py < 0 || py >= fb.height as i32 {
            continue;
        }
        let row = (fb.addr as usize + py as usize * fb.pitch as usize) as *mut u32;
        for dx in 0..cursor::W {
            let px = x + dx;
            let k = cursor::classify(dx, dy);
            if px < 0 || px >= fb.width as i32 || k == 0 {
                continue;
            }
            let p = row.add(px as usize);
            under[(dy * cursor::W + dx) as usize] = core::ptr::read_volatile(p);
            core::ptr::write_volatile(p, if k == 1 { cursor::OUTLINE } else { fill });
        }
    }
    DRAWN = Some((x, y));
}

/// The mouse moved (called from the PS/2 interrupt, or after a USB report):
/// move the pointer now, unless a frame is being copied.
pub fn moved(x: i32, y: i32) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    irq_off(|| unsafe {
        if BLITTING.load(Ordering::Relaxed) || !ACTIVE.load(Ordering::Relaxed) {
            return;
        }
        if DRAWN == Some((x, y)) {
            return;
        }
        restore();
        draw(x, y);
    });
}

/// Present is about to copy a frame: lift the pointer off (a frame smaller than
/// the screen would leave it behind) and keep the interrupt from drawing.
pub fn begin_blit() {
    irq_off(|| unsafe {
        BLITTING.store(true, Ordering::Relaxed);
        restore();
    });
}

/// The frame is on screen: draw the pointer on it at (x, y) physical pixels,
/// or keep it off (`show` false: the app draws its own, or a screenshot).
pub fn end_blit(x: i32, y: i32, show: bool) {
    irq_off(|| unsafe {
        DRAWN = None; // the frame overwrote whatever was there
        ACTIVE.store(show, Ordering::Relaxed);
        if show {
            draw(x, y);
        }
        BLITTING.store(false, Ordering::Relaxed);
    });
}

/// Something else wrote the whole screen (the WM's own present, a mode change):
/// the pointer pixels are gone, and the overlay stays off until the next app present.
pub fn forget() {
    irq_off(|| unsafe {
        DRAWN = None;
        ACTIVE.store(false, Ordering::Relaxed);
    });
}

/// Take the pointer off the screen (before reading the screen back).
pub fn hide() {
    irq_off(|| unsafe {
        restore();
        ACTIVE.store(false, Ordering::Relaxed);
    });
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}
