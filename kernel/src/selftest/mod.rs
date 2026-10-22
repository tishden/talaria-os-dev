//! The kernel's built-in self-checks (`talaria.test=<name>`).
//!
//! Talaria is not an application under an OS but the OS itself: its regression cannot be run
//! as `cargo test` on the host, because what must be checked is bringing the hardware up, the page
//! tables, the scheduler and ring 3. So every gate is A BOOT MODE:
//! `kernel/test.sh` boots the kernel in QEMU with `talaria.test=<name>` and asserts the output on
//! the serial port. Hence the form: not `#[test]` but a named mode.
//!
//! These ~90 modes used to live inside `rust_main` as a flat chain of
//! `if cmdline.contains("talaria.test=name")` 1300 lines long. Two troubles, both
//! closed here:
//!
//! 1. **A name that is a prefix started SOMEONE ELSE'S mode.** `contains("talaria.test=fs")`
//!    is true for `talaria.test=fsbig` too — and with `fsbig` the `fs` block really did
//!    execute (it went for `/hello.txt`, which is not on that volume). The class touched
//!    11 names; two got manual patches `&& !contains(..)`, and the rest
//!    silently depended on whether the previous block parks. Here the name is parsed
//!    once (`cmdline::test_name`) and compared IN FULL in a `match` —
//!    a collision is impossible by construction and the patches are unnecessary.
//! 2. **`rust_main` was unreadable.** Bringing the machine up was mixed with the regression.
//!    Now the bring-up is in `main.rs` and the regression is here, by topic.
//!
//! A typo in a name is no longer silent: an unknown name is printed to the log rather than
//! taking the run into the default production mode.

mod concur;
mod memory;
mod netsvc;
mod observ;
mod platform;
mod process;
mod storage;

/// The `talaria.test=` modes handled NOT by this module but by the production path
/// further down `rust_main` (they enable the network and go into the datapath). They are listed
/// so that the "unknown name" diagnostic does not complain about them.
const HANDLED_LATER: &[&str] = &["ipi", "connect", "txloss", "netshoot", "idleshoot", "forktls"];

/// Run the self-check named in `talaria.test=`.
///
/// Returns control if there is no name or the mode does not park — then
/// booting continues the ordinary way.
pub fn run(cmdline: &str) {
    let Some(name) = crate::cmdline::test_name(cmdline) else {
        return;
    };
    let handled = platform::run(name, cmdline)
        || storage::run(name, cmdline)
        || memory::run(name, cmdline)
        || process::run(name, cmdline)
        || concur::run(name, cmdline)
        || netsvc::run(name, cmdline)
        || observ::run(name, cmdline);
    if !handled && !HANDLED_LATER.contains(&name) {
        // The same class as an unknown knob: a typo in a gate's name used to
        // give a GREEN run (the mode did not start and the kernel went into the production branch).
        println!("talaria: talaria.test={} — there is no such mode (a typo?); the names are in kernel/src/selftest/*.rs", name);
        crate::cmdline::refuse(cmdline, "talaria.test");
    }
}

/// Whether a live observability gate is armed, and its report (called from `metrics::dump`).
pub fn obs_live_armed() -> bool { observ::live_armed() }
pub fn obs_live_report() { observ::live_report() }
/// The "ready->CPU" gate (`talaria.test=rqwait`), see `concur::rqwait_report`.
pub fn rqwait_armed() -> bool { concur::rqwait_armed() }
pub fn rqwait_report() { concur::rqwait_report() }
