//! A lock-free SPSC ring passing buffer ownership between shards
//! (index+length in one u64). head/tail on separate cache lines (no false sharing).
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(align(64))]
struct Pad(AtomicUsize);

pub struct Spsc {
    mask: usize,
    slots: Box<[UnsafeCell<u64>]>,
    head: Pad, // producer
    tail: Pad, // consumer
}
unsafe impl Sync for Spsc {}
unsafe impl Send for Spsc {}

impl Spsc {
    pub fn new(cap_pow2: usize) -> Self {
        assert!(cap_pow2.is_power_of_two());
        let slots = (0..cap_pow2).map(|_| UnsafeCell::new(0)).collect::<Vec<_>>().into_boxed_slice();
        Spsc { mask: cap_pow2 - 1, slots, head: Pad(AtomicUsize::new(0)), tail: Pad(AtomicUsize::new(0)) }
    }
    #[inline]
    pub fn push(&self, v: u64) -> bool {
        let h = self.head.0.load(Ordering::Relaxed);
        let t = self.tail.0.load(Ordering::Acquire);
        if h.wrapping_sub(t) > self.mask { return false; } // full
        unsafe { *self.slots[h & self.mask].get() = v; }
        self.head.0.store(h.wrapping_add(1), Ordering::Release);
        true
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.head.0.load(Ordering::Relaxed).wrapping_sub(self.tail.0.load(Ordering::Relaxed))
    }
    #[inline]
    pub fn pop(&self) -> Option<u64> {
        let t = self.tail.0.load(Ordering::Relaxed);
        let h = self.head.0.load(Ordering::Acquire);
        if t == h { return None; }
        let v = unsafe { *self.slots[t & self.mask].get() };
        self.tail.0.store(t.wrapping_add(1), Ordering::Release);
        Some(v)
    }
}
