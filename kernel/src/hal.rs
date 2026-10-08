//! virtio-drivers HAL for an identity-mapped single-address-space cell.
//! DMA memory comes from a static bump arena; phys == virt everywhere.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use virtio_drivers::{BufferDirection, Hal, PhysAddr};

const ARENA_SIZE: usize = 4 * 1024 * 1024;

#[repr(C, align(4096))]
struct Arena([u8; ARENA_SIZE]);

static mut DMA_ARENA: Arena = Arena([0; ARENA_SIZE]);
static DMA_NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct TalariaHal;

unsafe impl Hal for TalariaHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let bytes = pages * 4096;
        let off = DMA_NEXT.fetch_add(bytes, Ordering::Relaxed);
        assert!(off + bytes <= ARENA_SIZE, "DMA arena exhausted");
        let base = unsafe { core::ptr::addr_of_mut!(DMA_ARENA).cast::<u8>().add(off) };
        // Freshly used regions must be zeroed for virtqueue init.
        unsafe { core::ptr::write_bytes(base, 0, bytes) };
        (crate::mem::Pa::of(base).get() as PhysAddr, NonNull::new(base).unwrap())
    }

    unsafe fn dma_dealloc(_paddr: PhysAddr, _vaddr: NonNull<u8>, _pages: usize) -> i32 {
        0 // PoC: arena is never reclaimed
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(crate::mem::devptr::<u8>(crate::arch::mmio_va(paddr as u64))).unwrap() // arm64: GB0 through the devices' alias
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        crate::paging::virt_to_phys(crate::mem::vaddr(buffer.as_ptr())) as PhysAddr
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}
