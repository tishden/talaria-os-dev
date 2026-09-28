//! Host unit tests for the kernel (`cargo test` in this directory).
//!
//! The kernel is `no_std` on its own target, so its modules cannot be tested in
//! place. Modules whose logic does not touch hardware are compiled here a
//! second time, for the host, straight from `kernel/src` via `#[path]`: the
//! tests live next to the code (`#[cfg(test)] mod tests` in the module itself)
//! and exercise exactly the source the kernel builds. Kernel glue inside those
//! modules (page-table walks, port I/O, global state) is behind
//! `#[cfg(not(talaria_host))]` — `build.rs` sets that cfg for this crate only; what the pure code still names from `crate::` is
//! provided below as small stand-ins.
//!
//! A module joins this list when its logic can run without the machine — the
//! rule for new kernel code is to keep that logic in such a module.
#![allow(dead_code)]
// The same modules are compiled here as in the kernel, so the same lint decision
// applies — see the note at `#![allow(unsafe_op_in_unsafe_fn)]` in `kernel/src/main.rs`.
// A different edition or a different lint policy between the two builds would mean
// the tests judge code that is not quite the code the kernel runs.
#![allow(unsafe_op_in_unsafe_fn)]

// ── stand-ins for what the included modules name from `crate::` ──────────────

/// Kernel console output; on the host it goes to the test's stdout.
#[macro_export]
macro_rules! println {
    ($($t:tt)*) => { ::std::println!($($t)*) };
}

// ── kernel modules under test ────────────────────────────────────────────────

#[path = "../../kernel/src/cellman.rs"]
pub mod cellman;
#[path = "../../kernel/src/clock.rs"]
pub mod clock;
#[path = "../../kernel/src/cmdline.rs"]
pub mod cmdline;
#[path = "../../kernel/src/ext2hash.rs"]
pub mod ext2hash;
#[path = "../../kernel/src/ext2dir.rs"]
pub mod ext2dir;
#[path = "../../kernel/src/ext2dx.rs"]
pub mod ext2dx;
#[path = "../../kernel/src/ext2bmap.rs"]
pub mod ext2bmap;
#[path = "../../kernel/src/ext2sb.rs"]
pub mod ext2sb;
#[path = "../../kernel/src/fastring.rs"]
pub mod fastring;
#[path = "../../kernel/src/elfhdr.rs"]
pub mod elfhdr;
#[path = "../../kernel/src/elfnote.rs"]
pub mod elfnote;
#[path = "../../kernel/src/jbd2.rs"]
pub mod jbd2;
#[path = "../../kernel/src/crypto.rs"]
pub mod crypto;

#[path = "../../kernel/src/placement.rs"]
pub mod placement;
#[path = "../../kernel/src/routes.rs"]
pub mod routes;
#[path = "../../kernel/src/rqpick.rs"]
pub mod rqpick;
#[path = "../../kernel/src/tfs.rs"]
pub mod tfs;
#[path = "../../kernel/src/uaccess.rs"]
pub mod uaccess;
#[path = "../../kernel/src/waitpred.rs"]
pub mod waitpred;
