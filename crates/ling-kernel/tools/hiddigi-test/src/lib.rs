//! The kernel's `drivers/hiddigi.rs`, built on the host so its decoder can be tested against real
//! Wacom report descriptors (data/, from github.com/linuxwacom/wacom-hid-descriptors).
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

#[path = "../../../src/drivers/hiddigi.rs"]
pub mod hiddigi;
