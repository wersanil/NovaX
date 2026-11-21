//! Bridge between limine responses and `novax_memory` abstractions.
//!
//! All Limine requests are placed in the `.requests` section so the
//! bootloader can find them before `_start` runs.

use limine::request::{FramebufferRequest, HhdmRequest, MemmapRequest, MpRequest};
use limine::BaseRevision;
use novax_memory::{HhdmOffset, MemoryRegion, RegionKind};

/// Limine protocol revision NovaX requests (base revision 3).
#[used]
#[link_section = ".requests"]
pub static BASE_REVISION: BaseRevision = BaseRevision::with_revision(3);

/// Higher-half direct map request.
#[used]
#[link_section = ".requests"]
pub static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

/// Physical memory map request.
#[used]
#[link_section = ".requests"]
pub static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

/// Ask Limine for the full processor list (BSP and APs).
#[used]
#[link_section = ".requests"]
pub static MP_REQUEST: MpRequest = MpRequest::new(0);

/// Ask Limine to create a linear framebuffer (revision 0).
#[used]
#[link_section = ".requests"]
pub static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

/// Maximum number of memory-map entries copied into [`BootInfo`].
pub const MAX_REGIONS: usize = 128;

/// Size of the `FbInfo` wire message delivered to the `fb` server.
#[cfg_attr(feature = "test", allow(dead_code))]
pub const FB_INFO_LEN: usize = 48;
/// Magic number at the start of an `FbInfo` message ("NVFB").
#[cfg_attr(feature = "test", allow(dead_code))]
pub const FB_INFO_MAGIC: u32 = 0x4E56_4642;

/// Framebuffer geometry and physical address as reported by Limine.
#[cfg_attr(feature = "test", allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
    /// Physical base address of the linear framebuffer.
    pub paddr: u64,
    /// Number of bytes from `paddr` that belong to the framebuffer.
    pub len: u64,
    /// Horizontal resolution in pixels.
    pub width: u32,
    /// Vertical resolution in pixels.
    pub height: u32,
    /// Bytes per scanline (may exceed `width * bpp / 8`).
    pub pitch: u32,
    /// Bits per pixel.
    pub bpp: u16,
    /// Number of valid bits in the red channel.
    pub red_mask_size: u8,
    /// Bit offset of the red channel.
    pub red_mask_shift: u8,
    /// Number of valid bits in the green channel.
    pub green_mask_size: u8,
    /// Bit offset of the green channel.
    pub green_mask_shift: u8,
    /// Number of valid bits in the blue channel.
    pub blue_mask_size: u8,
    /// Bit offset of the blue channel.
    pub blue_mask_shift: u8,
}

#[cfg_attr(feature = "test", allow(dead_code))]
impl FramebufferInfo {
    /// Encode this geometry as the fixed 48-byte `FbInfo` IPC message.
    pub fn info_message(&self) -> [u8; FB_INFO_LEN] {
        let mut msg = [0u8; FB_INFO_LEN];
        msg[0..4].copy_from_slice(&FB_INFO_MAGIC.to_le_bytes());
        msg[4..8].copy_from_slice(&1u32.to_le_bytes());
        msg[8..12].copy_from_slice(&self.width.to_le_bytes());
        msg[12..16].copy_from_slice(&self.height.to_le_bytes());
        msg[16..20].copy_from_slice(&self.pitch.to_le_bytes());
        msg[20..22].copy_from_slice(&self.bpp.to_le_bytes());
        msg[22] = self.red_mask_size;
        msg[23] = self.red_mask_shift;
        msg[24] = self.green_mask_size;
        msg[25] = self.green_mask_shift;
        msg[26] = self.blue_mask_size;
        msg[27] = self.blue_mask_shift;
        // 28..32 reserved.
        msg[32..40].copy_from_slice(&self.paddr.to_le_bytes());
        msg[40..48].copy_from_slice(&self.len.to_le_bytes());
        msg
    }
}

/// Abort the boot if the running Limine does not support base revision 3.
pub fn check_base_revision() {
    if !BASE_REVISION.is_supported() {
        panic!(
            "limine base revision not supported: requested {}, actual {:?}",
            BaseRevision::MAX_SUPPORTED,
            BASE_REVISION.actual_revision()
        );
    }
}

/// Snapshot of the bootloader data the kernel needs after `_start`.
pub struct BootInfo {
    /// HHDM offset used for all physical memory access.
    pub hhdm: HhdmOffset,
    /// Fixed-capacity copy of the memory map.
    pub regions: [MemoryRegion; MAX_REGIONS],
    /// Number of valid entries in `regions`.
    pub region_count: usize,
    /// Linear framebuffer, if the bootloader created one.
    pub framebuffer: Option<FramebufferInfo>,
}

impl BootInfo {
    /// The valid prefix of [`BootInfo::regions`].
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions[..self.region_count]
    }
}

/// Copy the HHDM offset and memory map out of the Limine responses.
///
/// Panics if Limine did not provide the requested responses: the kernel
/// cannot run without a direct map or a memory map.
pub fn collect() -> BootInfo {
    let hhdm = HHDM_REQUEST.response().expect("limine: no HHDM response");
    let mmap = MEMMAP_REQUEST
        .response()
        .expect("limine: no memory map response");

    let mut regions = [MemoryRegion {
        base: 0,
        length: 0,
        kind: RegionKind::Reserved,
    }; MAX_REGIONS];
    let mut count = 0;

    for entry in mmap.entries() {
        if count >= MAX_REGIONS {
            break;
        }
        // limine 0.6.x exposes `type_` as a raw u64; values match the
        // limine protocol spec (0 = USABLE, 1 = RESERVED, ...).
        let kind = match entry.type_ {
            0 => RegionKind::Usable,
            1 => RegionKind::Reserved,
            2 => RegionKind::AcpiReclaimable,
            3 => RegionKind::AcpiNvs,
            4 => RegionKind::BadMemory,
            5 => RegionKind::BootloaderReclaimable,
            6 => RegionKind::ExecutableAndModules,
            7 => RegionKind::Framebuffer,
            _ => RegionKind::Reserved,
        };
        regions[count] = MemoryRegion {
            base: entry.base,
            length: entry.length,
            kind,
        };
        count += 1;
    }

    let framebuffer = FRAMEBUFFER_REQUEST.response().and_then(|response| {
        let fb = response.framebuffers().first().copied()?;
        let address = fb.address() as u64;
        if address == 0 || fb.width == 0 || fb.height == 0 || fb.pitch == 0 {
            return None;
        }

        // Limine reports the framebuffer through the HHDM. Fall back to the
        // raw address if a bootloader variant already gave us a physical one.
        let paddr = if address >= hhdm.offset {
            address - hhdm.offset
        } else {
            address
        };
        let len = fb.height.checked_mul(fb.pitch)?;
        // Keep the capability window sane; 8K RGBA is 256 MiB.
        if len == 0 || len > 256 * 1024 * 1024 {
            return None;
        }

        Some(FramebufferInfo {
            paddr,
            len,
            width: u32::try_from(fb.width).ok()?,
            height: u32::try_from(fb.height).ok()?,
            pitch: u32::try_from(fb.pitch).ok()?,
            bpp: fb.bpp,
            red_mask_size: fb.red_mask_size,
            red_mask_shift: fb.red_mask_shift,
            green_mask_size: fb.green_mask_size,
            green_mask_shift: fb.green_mask_shift,
            blue_mask_size: fb.blue_mask_size,
            blue_mask_shift: fb.blue_mask_shift,
        })
    });

    BootInfo {
        hhdm: HhdmOffset(hhdm.offset),
        regions,
        region_count: count,
        framebuffer,
    }
}
