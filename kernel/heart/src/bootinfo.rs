//! Bridge between limine responses and `novax_memory` abstractions.
//!
//! All Limine requests are placed in the `.requests` section so the
//! bootloader can find them before `_start` runs.

use limine::request::{HhdmRequest, MemmapRequest, MpRequest};
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

/// Maximum number of memory-map entries copied into [`BootInfo`].
pub const MAX_REGIONS: usize = 128;

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

    BootInfo {
        hhdm: HhdmOffset(hhdm.offset),
        regions,
        region_count: count,
    }
}
