//! Minimal PCI configuration-space access.
//!
//! q35 exposes the legacy 0xCF8/0xCFC mechanism and keeps every device we
//! care about on the single root bus, so a full ECAM/MCFG parser would be
//! dead weight here. Enumeration is read-only except for the BAR sizing
//! probe, which restores the original value immediately.

use core::arch::asm;

const CONFIG_ADDR: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Transitional virtio-net (legacy I/O BAR). Scripts boot QEMU with
/// `disable-modern=on`, which pins the device to this ID.
const VIRTIO_NET_LEGACY: u16 = 0x1000;

const PCI_COMMAND: u8 = 0x04;
const PCI_BAR0: u8 = 0x10;
const PCI_INTERRUPT_LINE: u8 = 0x3C;

const CMD_IO_SPACE: u16 = 1 << 0;
const CMD_BUS_MASTER: u16 = 1 << 2;

/// Where a legacy virtio-net device lives, in capability terms.
#[derive(Clone, Copy, Debug)]
pub struct VirtioNet {
    /// First I/O port of BAR0. Userspace only sees offsets into this window.
    pub io_base: u16,
    /// Window length in bytes (32 for legacy virtio-net).
    pub io_len: u16,
    /// PCI interrupt line; kept for logging and future IRQ-driven mode.
    #[allow(dead_code)]
    pub irq: u8,
}

/// # Safety
///
/// Privileged port I/O; the caller must own the target PCI config port.
#[inline]
unsafe fn outl(port: u16, value: u32) {
    // WHY unsafe: privileged port I/O, caller owns the target register.
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
    }
}

/// # Safety
///
/// Privileged port I/O; the caller must know the port is readable.
#[inline]
unsafe fn inl(port: u16) -> u32 {
    // WHY unsafe: privileged port I/O, caller knows the port is readable.
    let value: u32;
    unsafe {
        asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    // WHY unsafe: port I/O is privileged; the address is fully specified
    // by the caller and the function only reads.
    unsafe {
        outl(CONFIG_ADDR, config_address(bus, device, function, offset));
        inl(CONFIG_DATA)
    }
}

fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    // WHY unsafe: same as `read32`, but the caller must intend the write.
    unsafe {
        outl(CONFIG_ADDR, config_address(bus, device, function, offset));
        outl(CONFIG_DATA, value);
    }
}

fn read16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    (read32(bus, device, function, offset & !1) >> ((offset & 2) * 8)) as u16
}

fn read8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    (read32(bus, device, function, offset & !3) >> ((offset & 3) * 8)) as u8
}

fn write16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let shift = (offset & 2) * 8;
    let old = read32(bus, device, function, offset & !3);
    let merged = (old & !(0xFFFF << shift)) | ((value as u32) << shift);
    write32(bus, device, function, offset & !3, merged);
}

/// Size an I/O BAR by writing all-ones and reading the writable bits back.
/// The original value is restored before returning.
fn io_bar_size(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let original = read32(bus, device, function, offset);
    write32(bus, device, function, offset, 0xFFFF_FFFF);
    let mask = read32(bus, device, function, offset);
    write32(bus, device, function, offset, original);

    let usable = mask & !0x3; // bits 1:0 are read-only for I/O BARs
    if usable == 0 {
        return 0;
    }
    (!usable).wrapping_add(1) as u16
}

/// Find the transitional virtio-net device on the root bus.
pub fn find_virtio_net() -> Option<VirtioNet> {
    for device in 0..32u8 {
        let vendor = read16(0, device, 0, 0x00);
        if vendor == 0xFFFF {
            continue;
        }
        if vendor != VIRTIO_VENDOR {
            continue;
        }

        let id = read16(0, device, 0, 0x02);
        if id != VIRTIO_NET_LEGACY {
            continue;
        }

        let bar0 = read32(0, device, 0, PCI_BAR0);
        if bar0 & 1 == 0 {
            // A memory BAR means modern-only virtio; the legacy register
            // layout this driver speaks is not available.
            continue;
        }

        let io_base = (bar0 & !0x3) as u16;
        let io_len = io_bar_size(0, device, 0, PCI_BAR0);
        let irq = read8(0, device, 0, PCI_INTERRUPT_LINE);

        let command = read16(0, device, 0, PCI_COMMAND);
        write16(
            0,
            device,
            0,
            PCI_COMMAND,
            command | CMD_IO_SPACE | CMD_BUS_MASTER,
        );

        crate::serial_println!(
            "pci: virtio-net 1af4:{:04x} io=0x{:04x}+0x{:x} irq={}",
            id,
            io_base,
            io_len,
            irq
        );
        return Some(VirtioNet {
            io_base,
            io_len,
            irq,
        });
    }
    None
}
