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
        let base = core::ptr::addr_of_mut!(DMA_ARENA) as usize + off;
        // Freshly used regions must be zeroed for virtqueue init.
        unsafe { core::ptr::write_bytes(base as *mut u8, 0, bytes) };
        (base as PhysAddr, NonNull::new(base as *mut u8).unwrap())
    }

    unsafe fn dma_dealloc(_paddr: PhysAddr, _vaddr: NonNull<u8>, _pages: usize) -> i32 {
        0 // PoC: arena is never reclaimed
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        crate::paging::virt_to_phys(buffer.as_ptr() as *mut u8 as u64) as PhysAddr
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}
