//! The network layer of ring-3 socket syscalls (smoltcp on top of any `NetDev` card).
//!
//! v2: a full-blown BSD model of sockets instead of one listener pool:
//! - NS independent TCP sockets with their own buffers: a listener (listen/accept with
//!   a pool of backlog slots per port — several connections at once) AND
//!   a client (an OUTGOING connect: blocking and non-blocking/EINPROGRESS,
//!   SO_ERROR, an EPOLLOUT readiness at the establishment);
//! - NU UDP sockets (musl's DNS resolver: socket/bind(0)/sendto/poll/recvfrom);
//! - several epoll instances + a poll(2) on top of a common readiness mask;
//! - the blocking calls give the processor to other threads (an sti window between the polls).
//! The fd ranges: TCP 100..100+NS, UDP 50..50+NU, epoll 200..200+NE.
//!
//! v3 (V1.11): SEVERAL INTERFACES. Before that the stack was one for the whole machine:
//! one card, one address, one gateway. That does not suit a node with two ENIs (a management network +
//! a working one) at all, and it is exactly like that that they are deployed.
//!
//! WHY EVERY INTERFACE HAS ITS OWN `SocketSet` RATHER THAN A COMMON ONE. In smoltcp
//! `Interface::poll` goes through ALL the sockets of the set and sends a packet if THIS
//! interface has found a route to the addressee; it does not check that the source address coincides
//! with the interface's addresses. A common set for two interfaces with a default
//! route at each is one and the same segment that went into both
//! cards, with a foreign source address in the second. So a socket IS TIED to an
//! interface: the interface is chosen by the routing table (`netcfg`) at the moment of the
//! `connect`, while a listener lays its backlog slots out over ALL the brought-up
//! interfaces — that way `listen(0.0.0.0)` still means "on all of them".
use core::ptr::addr_of_mut;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::socket::tcp::{self, State};
use smoltcp::socket::udp;
use smoltcp::socket::dhcpv4;
use smoltcp::wire::{IpEndpoint, IpListenEndpoint};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use crate::l1::SmolDev;
use crate::tsc;

// THE TCP SOCKETS PER MACHINE. Sixteen is too few: a listener spends one slot on the socket
// itself and eight on the backlog (`BACKLOG`), that is, seven were left for the ACCEPTED
// connections. The suite on EC2 by default opens eight
// (`run-remote.sh`, CONNS=8) — that is, it would have run into the ceiling at the very first
// run, and silently at that: `arm()` would stop replenishing the backlog, the SYNs
// would stay unanswered, and the client would HANG (now there is a line about that, see
// `warn_no_slots`).
//
// Sixty-four rather than thirty-two: a slot costs buffers (128 KB of receive +
// 64 KB of transmit), that is, doubling the table is another 6 MB of .bss. Thirty-two
// were enough exactly until the connections were short: with PostgreSQL a process is brought up for
// every connection, and in the validation `B10` the table ran into
// the ceiling ENTIRELY (`32 out of 32 are occupied`) — after which the listener's backlog was no longer
// replenished and the client HUNG. Six megabytes of .bss are a price worth
// paying so that the machine's ceiling does not turn out to be lower than the load on the EC2
// stand, where reproducing and analysing that would be the most expensive of all.
pub const NS: usize = 64;        // the TCP sockets
pub const NU: usize = 8;         // the UDP sockets
pub const NE: usize = 32;        // the epoll instances (one per PG process)
pub const MAXIF: usize = 4;      // the interfaces (cards) per machine
pub const TCP_BASE: i64 = 100;
pub const UDP_BASE: i64 = 50;
pub const EP_BASE: i64 = 200;
const RXB_SZ: usize = 128 * 1024; // the TCP rx buffer (the window; the WS/TLS bursts of market data)
const TXB_SZ: usize = 64 * 1024;  // the TCP tx buffer
const BACKLOG: usize = 8;         // the backlog slots in Listen per listener (parallel connects)
const UPKT: usize = 512;          // the UDP packet of metadata per socket (the bursts)
const UBUF: usize = 512 * 1024;   // the UDP buffer of bytes per socket

// errno
const EAGAIN: i64 = -11;
const EBADF: i64 = -9;
const EINVAL: i64 = -22;
const EINPROGRESS: i64 = -115;
const ECONNREFUSED: i64 = -111;
const ETIMEDOUT: i64 = -110;
const ENOTCONN: i64 = -107;
const EISCONN: i64 = -106;
const EMFILE: i64 = -24;
const EADDRINUSE: i64 = -98;

#[derive(Clone, Copy, PartialEq, Eq)]
enum K { Free, Unbound, Listen, Backlog, Conn, Closing }

#[derive(Clone, Copy)]
struct Tcp {
    k: K,
    nonblock: bool,
    port: u16,          // the local port (bind / the listener)
    owner: u8,          // Backlog: the index of the listener's slot
    connecting: bool,   // a non-blocking connect is in progress
    err: i32,           // a deferred error (SO_ERROR)
    nodelay: bool,
    rx: u64, tx: u64,   // the bytes received/sent (the connections' trace)
    ifx: u8,            // ON WHICH interface the socket lives
    // WHO OWNS THE SOCKET. Without that `netstat` answers "the port 9000 is being listened on", but
    // does not answer "by whom", while on a machine with one application and a dozen of its threads
    // it is exactly the second that is needed: was the listener opened by the receive thread or by the logging thread.
    tid: u8,
    h: Option<SocketHandle>, // the handle in this interface's set (None — it is not bound)
    /// WHEN THE DATA APPEARED IN THIS SOCKET (the TSC cycle), 0 — they have already been given to the reader.
    /// Only under `talaria.netprof=`: the difference with the moment of the exit from `read` is exactly
    /// the latency of THE DELIVERY TO THE READER — the only stage that on a multi-threaded
    /// server can be measured without gluing the turnarounds of different connections.
    rdy: u64,
    /// The call to the owner about THIS appearance of data has already been made (see `deliver_ready`).
    armed: bool,
    /// The instrument "the delivery -> the reader by the reader's state" (session 32, see `dl_class`):
    /// the delivery's cycle, the class of the owner's state and a snapshot of its core at that moment.
    dl: u64,
    dlx: u64,
    dlc: u8,
    dlo: u8,
    dlr: u64,
    dls: u64,
    dlp: u64,
    /// the reader's core and the deliverer's core at the moment of the delivery (session 32's instrument)
    dlcpu: u8,
    dlby: u8,
    /// A snapshot of the counters of THE ONE OCCUPYING the reader's core at the moment of the delivery: the time by
    /// phase (`NW_PH_T`) and the outcomes of the yield window (`NW_WHY`) — the difference in
    /// `dl_dump` says WHERE the occupant spent the wait for the reader.
    dlpt: [u64; 16],
    dlwhy: [u32; WHY_N],
    /// THE SERVER'S TURNAROUND ON A SOCKET (session 32, step 1.1): the delivery's cycle, the cycle
    /// of the return from `read`, the delivery->read in us and the estimate "the ring->the pass" for
    /// that frame; it is closed in `write` (`tr_close`).
    trd: u64,
    trrd: u64,
    trdus: u32,
    trdla: u32,
}
// `K::Closing` IS AN ORPHAN: the descriptor is already closed, while the FIN handshake is still going.
// Previously `close()` waited for it itself for up to 200 ms, spinning on the core UNDER THE STACK'S LOCK,
// and on the stand that read as the load generator's debt of exactly 200 ms: one
// peer that did not answer — and all the machine's connections stand for a fifth of a second.
// Now close() sends a FIN and returns, while the slot waits for the untying in
// a pass of the stack (`reap_orphans`); an orphan's `rdy` is the term (a TSC cycle) after
// which we stop waiting and send an RST. The slot is occupied until the end of the handshake — that
// is, for one network round trip with a live peer rather than for 200 ms with a dead one.
//
// THE TERM COUNTS FROM THE LAST PROGRESS rather than from the close(). The first version counted
// 200 ms by the clock, and on EC2 the generator's report (20-50 KB) to a peer across
// the Internet (an RTT of 0.25 s) did not manage to leave: an RST flew by the term, the harness got
// a ConnectionResetError in the middle of a read — the point was lost, although the job had been
// done. An orphan's `tx` is the length of the send queue at the last check
// (the statistics have already been handed over to TOT_TX), `rdy` is the term of silence: while the queue
// decreases, the term is moved; `rx` is the absolute ceiling, so that a peer
// that reads a byte at a time does not hold the slot forever.
const ORPHAN_IDLE_MS: i64 = 1000;   // without progress for longer — an RST (the Internet's RTT of 0.25 s — with headroom)
const ORPHAN_CAP_MS: i64 = 10_000;  // for the whole untying — no more than that

const TCP0: Tcp = Tcp { k: K::Free, nonblock: false, port: 0, owner: 0, connecting: false, err: 0, nodelay: false, rx: 0, tx: 0, ifx: 0, tid: 255, h: None,
    rdy: 0, armed: false, dl: 0, dlx: 0, dlc: 0, dlo: 0, dlr: 0, dls: 0, dlp: 0, dlcpu: 255, dlby: 255, dlpt: [0; 16], dlwhy: [0; WHY_N], trd: 0, trrd: 0,
    trdus: 0, trdla: 0 };
// ── the common lock of the network stack ──────────────────────────────────────
//
// smoltcp's interfaces, the table of sockets and the card's rings are mut statics and RefCells.
// While the core was one, that was enough by construction. With several cores two
// PostgreSQL backends, each sitting in its own socket syscall, entered the stack
// AT THE SAME TIME, and the RefCell caught that with a panic "RefCell already borrowed" — THE KERNEL
// fell rather than the application.
//
// The owner is A CPU CORE rather than a task. Ownership by a task looks more natural
// (a task is preempted inside the kernel too, and it may reach another core together with
// the lock), but it breaks against two places at once:
//   * the console works FROM A TICK on top of a live workload; a tick interrupts that same
//     task in the middle of working with the stack, and "the same task number" would mean
//     a permission to enter as a second one — on top of a live borrow;
//   * the current task's number on a core without a raised gs falls back to the common
//     CURRENT, and two cores see ONE number — that is, they let each other in.
//
// Ownership by a core is correct only together with a prohibition of preemption for the time of
// the holding: otherwise the task will move away while the lock stays "occupied by this core". The prohibition
// costs little — the section is short (one pass of the stack) and bounded in time.
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64};

static NET_OWNER: AtomicI64 = AtomicI64::new(-1);   // the owner core, -1 is free
static NET_DEPTH: AtomicU32 = AtomicU32::new(0);    // the recursion inside one chain of calls
/// How many times the wait for the lock spun (it is visible in `kstat net`): it grows — the cores
/// fight for the stack, and that is a question to the threads' layout rather than to the lock itself.
static NET_SPINS: AtomicU64 = AtomicU64::new(0);
/// How many times an observing path (the console from a tick) backed off without getting
/// the lock: that is not an error but a refusal to get in the datapath's way.
static NET_SKIPS: AtomicU64 = AtomicU64::new(0);
/// Is the lock held on this core — by this flag the scheduler does NOT preempt.
static NET_HELD: [AtomicU32; crate::smp::MAXCPU] =
    [const { AtomicU32::new(0) }; crate::smp::MAXCPU];

/// Does the core `cpu` hold the stack's lock (the scheduler asks before a preemption).
pub fn holds_lock(cpu: usize) -> bool {
    cpu < crate::smp::MAXCPU && NET_HELD[cpu].load(core::sync::atomic::Ordering::Acquire) != 0
}

pub struct NetGuard(i32);
impl Drop for NetGuard {
    fn drop(&mut self) {
        if self.0 < 0 { return; }
        use core::sync::atomic::Ordering::*;
        let cpu = self.0 as usize;
        let last = NET_DEPTH.fetch_sub(1, AcqRel) == 1;
        if last {
            note_released();               // the holding time belongs only to the OUTER ownership
            // SeqCst: the store and the load of `NIC_PENDING` below are one half of a Dekker
            // pair with `nic_irq` (its flag, then its retry of the lock). With Release here the
            // load could be satisfied before the store is visible — the handler's try fails
            // on the old owner, our load misses its flag, and the frame waits for the ceiling.
            NET_OWNER.store(-1, SeqCst);
        }
        if cpu < crate::smp::MAXCPU { NET_HELD[cpu].fetch_sub(1, AcqRel); }
        // THE PASS'S DEBT IS PAID BY THE ONE WHO RELEASED THE LOCK. The line's handler takes
        // the lock BY AN ATTEMPT: it is busy — the frame stays in the ring, while the sign
        // `NIC_PENDING` says "somebody must finish the pass". Waiting for that
        // "somebody" took up to the parking ceiling (1 ms), and on a workload with
        // a thread per connection (MariaDB: the stack's lock is busy almost always)
        // the latency fell on EVERY such frame — in the console that is visible as
        // thousands of "Aborted connection ... error reading communication packets".
        // Paying the debt is cheapest right here: we have just left the section, the stack
        // is free, and the pass will cost one entry by recursion.
        if last && NIC_PENDING.load(SeqCst) {
            NIC_PENDING_RUN.fetch_add(1, Relaxed);
            poll_forced();
        }
    }
}

/// Mark the release of the OUTER ownership (it is called from `Drop` when the depth
/// has reached zero): only that is the holding time.
#[inline]
fn note_released() {
    use core::sync::atomic::Ordering::Relaxed;
    let t0 = NET_SINCE.load(Relaxed);
    if t0 == 0 { return; }
    let d = tsc::rdtsc().wrapping_sub(t0);
    NET_HELD_TSC.fetch_add(d, Relaxed);
    // THE HISTOGRAM OF THE HOLDING. One "worst" does not judge: the maximum is ONE
    // event, and it may turn out to be both the host's departure of the vCPU and a real long
    // section. They are told apart by the frequency: how many holdings are longer than 100 us and WHO
    // makes them — and that is exactly the answer to "the lock is expensive because it is held
    // long — but who holds it?". That is how the printing of the trace
    // `net: close` UNDER THE LOCK was found (see `NET_TRACE`): all the holdings longer than
    // a millisecond were from close(), and not one of them was a pass of the stack.
    let hz = unsafe { HZ }.max(1);
    let us = d.saturating_mul(1_000_000) / hz;
    let b = if us < 10 { 0 } else if us < 100 { 1 } else if us < 1000 { 2 } else { 3 };
    NET_HELD_HIST[b].fetch_add(1, Relaxed);
    if b >= 2 {
        // The long holdings go by place: a syscall (the number) or the context (a tick, the line).
        let t = crate::sched::current_id();
        let cpu = crate::percpu::cpu_id();
        let key: u32 = if crate::console::in_tick() { 0x1_0000 }
            else if cpu < crate::smp::MAXCPU && IN_NIC_IRQ[cpu].load(Relaxed) != 0 { 0x2_0000 }
            else if t < crate::sched::NTASKS { (crate::syscall::LAST_SYS[t].load(Relaxed) >> 48) as u32 } else { 0xffff };
        for e in NET_HELD_LONG.iter() {
            let k = e.0.load(Relaxed);
            if k == key || (k == u32::MAX && e.0.compare_exchange(u32::MAX, key, Relaxed, Relaxed).is_ok()) {
                e.1.fetch_add(1, Relaxed); e.2.fetch_add(us, Relaxed); break;
            }
        }
    }
    if d > NET_HELD_MAX.load(Relaxed) {
        NET_HELD_MAX.store(d, Relaxed);
        // WHO held it: the task, its syscall and the context. A new maximum is a rare
        // event, so the price of this record on the datapath is zero.
        let t = crate::sched::current_id();
        let sc = if t < crate::sched::NTASKS { crate::syscall::LAST_SYS[t].load(Relaxed) >> 48 } else { 0xffff };
        let cpu = crate::percpu::cpu_id();
        let ctx = if crate::console::in_tick() { 1 } else if cpu < crate::smp::MAXCPU && IN_NIC_IRQ[cpu].load(Relaxed) != 0 { 2 } else { 0 };
        NET_HELD_MAX_WHO.store((t as u64) | (sc << 16) | ((cpu as u64) << 32) | (ctx << 40), Relaxed);
    }
}

/// THE ACCOUNTING OF THE STACK'S LOCK is the same as at the FS lock, and for the same reason.
///
/// Before that only the SPIN TURNS were counted, while by a turn one cannot answer the question
/// that decides: is the lock expensive because it is WAITED FOR LONG or because it is HELD
/// LONG? That is cured differently — in the first case with a better lock, in the second
/// by shortening the critical section (or splitting it) — and choosing at random
/// here is not allowed: the section lies on the datapath.
///
/// Only an OUTER take is counted: a recursion inside one chain of calls
/// does not prolong the holding.
pub static NET_TAKES: AtomicU64 = AtomicU64::new(0);
pub static NET_CONT: AtomicU64 = AtomicU64::new(0);
pub static NET_WAIT_TSC: AtomicU64 = AtomicU64::new(0);
pub static NET_HELD_TSC: AtomicU64 = AtomicU64::new(0);
pub static NET_HELD_MAX: AtomicU64 = AtomicU64::new(0);
/// Who held it the longest: the task | the syscall<<16 | the core<<32 | the context<<40
/// (0 — a syscall, 1 — a tick, 2 — the card's line).
pub static NET_HELD_MAX_WHO: AtomicU64 = AtomicU64::new(0);
/// The moment of the take by the outer owner (the owner core is one, so it is one field).
static NET_SINCE: AtomicU64 = AtomicU64::new(0);
/// The holdings by duration: <10 us, <100 us, <1 ms, >=1 ms.
pub static NET_HELD_HIST: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// The long holdings (>=100 us) by place: (the key, how many, in total us);
/// the key is the syscall's number, 0x10000 a tick, 0x20000 the card's line, u32::MAX — empty.
pub static NET_HELD_LONG: [(AtomicU32, AtomicU64, AtomicU64); 8] =
    [const { (AtomicU32::new(u32::MAX), AtomicU64::new(0), AtomicU64::new(0)) }; 8];
/// Is the card's line handler running on this core (a prohibition of recursion into the lock).
static IN_NIC_IRQ: [AtomicU32; crate::smp::MAXCPU] =
    [const { AtomicU32::new(0) }; crate::smp::MAXCPU];
/// How many interrupts from the card have been handled and how many of them found the lock busy.
pub static NIC_IRQS: AtomicU64 = AtomicU64::new(0);
pub static NIC_IRQ_BUSY: AtomicU64 = AtomicU64::new(0);
/// AN INTERRUPT WHOSE PASS DID NOT TAKE PLACE MUST NOT BE LOST.
///
/// The handler takes the lock BY AN ATTEMPT: it is busy — which means somebody
/// else is driving the stack, and previously that was enough (the one inside would take our frame
/// too). With the parking it stopped being so: the lock's owner could enter its pass BEFORE the
/// frame arrived, leave, find nobody and fall asleep — while the frame stayed in the ring.
/// There will be no new interrupt for it (the acknowledgement has already been removed), and the waiter
/// sleeps through the parking's ceiling. The measurement showed exactly that: 361 654 passes out of 625 616
/// did not take place, while a request's p99 grew from 250 us to 67 ms.
///
/// The flag closes the hole: a missed pass must be made by the one who takes the lock
/// the next time, and certainly by a waiter BEFORE it falls asleep.
static NIC_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// How many times a missed pass was finished later (the diagnostics of the hole above).
pub static NIC_PENDING_RUN: AtomicU64 = AtomicU64::new(0);

/// `talaria.netirq=1` — switch the interrupt from the card on through the I/O APIC.
///
/// THE LINE'S NUMBER. Under microvm the virtio-mmio windows stand on a fixed grid
/// (0xFEB0_0000 + 512*i), while the lines are handed out in the same order starting with the fifth
/// — that is, the GSI is derived from the slot's number, and there is no need to guess. On a machine with
/// firmware that describes the devices differently, the knob is simply not switched on:
/// the default is as it was, the stack from the syscalls.
pub static NETIRQ_GSI: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(u32::MAX);
/// The line is not a GSI but an MSI-X vector (ENA): `TAG | the vector` lies in `NETIRQ_GSI`.
pub const NETIRQ_MSI_TAG: u32 = 0x1000;
/// How to name the line in the counters: "GSI n", "MSI-X 0x65", "none".
pub fn netirq_line_name() -> LineName { LineName(NETIRQ_GSI.load(core::sync::atomic::Ordering::Relaxed)) }
pub struct LineName(u32);
impl core::fmt::Display for LineName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            u32::MAX => write!(f, "none"),
            g if g == u32::MAX - 1 => write!(f, "all the GSIs"),
            g if g & NETIRQ_MSI_TAG != 0 => write!(f, "MSI-X {:#x}", g & !NETIRQ_MSI_TAG),
            g => write!(f, "GSI {}", g),
        }
    }
}

/// Lay the card's line if it was asked for. It is called after the interface is brought up.
pub static NETIRQ_WANTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// THE LINE'S MODE: 0 — it was not asked for (polling), 1 — it was asked for, 2 — it was FORBIDDEN explicitly.
///
/// The third value appeared because there was no switch at all: any
/// value of the knob meant "switch it on", and `talaria.netirq=0` SWITCHED the line ON.
///
/// WE DO NOT CHANGE THE DEFAULT YET, and that is a decision of a measurement rather than of caution. On ENA
/// (MSI-X, a message interrupt) the A/B of 6-7 September took two workloads in different
/// directions: mongodb was BETTER at 37 points out of 40 (the p99 of `ping` 134 -> 67 us, of
/// `find` at 12825 req/s 1000 -> 138 us, and the refusal to accept connections after a closed
/// loop of the ceiling disappeared), mysql was WORSE at 20 out of 20 (the p99 of `pk` 100 us ->
/// 1.5-5.1 ms with an unchanged p50). mysql's "after" plateau is suspiciously similar
/// to the parking's ceiling (1 ms), while `talaria.netirq=1` switches on THE LINE AND THE PARKING
/// at once: MariaDB is a thread per connection with a blocking `read`, mongod is a reactor
/// on an epoll. Which means the likely culprit is not the line but the wakeup
/// of a parked blocking reader. The check (the line WITHOUT the parking,
/// `talaria.netirq=1 talaria.netpark=0`) is in the queue; until its result
/// the default is polling, as it was.
pub static NETIRQ_MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
pub static NETIRQ_ALL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub static NETIRQ_FORCE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static NETIRQ_FROM: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(u32::MAX);
pub static NETIRQ_TO: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub fn netirq_setup() {
    use core::sync::atomic::Ordering::SeqCst;
    let mode = NETIRQ_MODE.load(SeqCst);
    if mode != 1 { return; }                            // 0 — it was not asked for, 2 — `talaria.netirq=0`
    if NETIRQ_GSI.load(SeqCst) != u32::MAX { return; } // it has already been laid
    // A card with a message line (ENA on EC2): an MSI-X vector instead of a GSI. Before
    // session 26 `talaria.netirq=1` on EC2 answered "the card is not on the microvm grid",
    // and a frame that arrived at an empty machine waited for the TICK of the core with a network waiter
    // (`netslice` = 100 us): with mongodb's `find` at 1.2 thousand req/s that is a separate
    // mode of the latency of exactly +100 us at every tenth request.
    if let Some(f) = ifopt(0) {
        if let Some(v) = f.dev.irq_vector() {
            crate::idt::set_vector(v as usize, crate::idt::nic_handler());
            NETIRQ_GSI.store(NETIRQ_MSI_TAG | v as u32, SeqCst);
            f.dev.arm_intr();
            crate::println!("netsys: the card's line is the MSI-X vector {:#x} -> the line's handler (the core by the layout irq=)", v);
            return;
        }
    }
    if !crate::ioapic::available() {
        crate::println!("netsys: talaria.netirq=1, but the I/O APIC was not found in the MADT — the line is not laid");
        return;
    }
    let apic = crate::smp::apic_of(0);
    let (lo, hi) = (NETIRQ_FROM.load(SeqCst), NETIRQ_TO.load(SeqCst));
    if lo != u32::MAX {
        let mut n = 0;
        for g in lo..=hi { if crate::ioapic::route(g, crate::idt::NIC_VECTOR as u8, apic) { n += 1; } }
        NETIRQ_GSI.store(lo, SeqCst);
        if let Some(f) = ifopt(0) { f.dev.arm_intr(); }
        crate::println!("netsys: the lines {}..{} ({} of them) have been laid -> the vector {:#x}", lo, hi, n, crate::idt::NIC_VECTOR);
        return;
    }
    if NETIRQ_ALL.load(SeqCst) {
        // A bisect: we lay EVERYTHING there is and look at whether anything comes at all.
        // The line's number from the slot is an assumption about QEMU's layout, and checking it
        // must be done by a measurement rather than by reading somebody else's sources from memory.
        let mut n = 0;
        for g in 0..48u32 { if crate::ioapic::route(g, crate::idt::NIC_VECTOR as u8, apic) { n += 1; } }
        NETIRQ_GSI.store(u32::MAX - 1, SeqCst);
        if let Some(f) = ifopt(0) { f.dev.arm_intr(); }
        crate::println!("netsys: ALL the lines ({}) have been laid -> the vector {:#x} (a bisect)", n, crate::idt::NIC_VECTOR);
        return;
    }
    let forced = NETIRQ_FORCE.load(SeqCst);
    let gsi = if forced != 0 { forced } else {
        match crate::virtnet::mmio_slot() {
            // HOW THE LINE'S NUMBER IS DERIVED. The naive "the fifth plus the slot's number" is
            // wrong, and that cost a bisect: with a microvm with two controllers
            // the virtio lines lie at the TOP of the GSI space. By measurement: a card in
            // slot 23 sits on GSI 47 (179 thousand interrupts), a disk in slot 22 is
            // on 46, while GSI 28 ("5 + 23") is silent. So the anchor is taken from the top:
            // THE HIGHEST occupied slot corresponds to THE HIGHEST line, and the rest are
            // counted from it. If the layout turns out to be different, the number
            // is set by hand (`talaria.netirq=<gsi>`), while `=all` lays everything.
            // That is x86's `arch::virtio_mmio_gsi`; on arm64 the device tree NAMES the SPI of
            // every window and nothing is derived.
            Some(i) => match crate::arch::virtio_mmio_gsi(i) {
                Some(g) => g,
                None => { crate::println!("netsys: talaria.netirq: the lines' layout was not derived — set the number by hand"); return; }
            },
            None => { crate::println!("netsys: talaria.netirq: the card is not on the microvm grid — the line is not laid"); return; }
        }
    };
    if crate::ioapic::route(gsi, crate::idt::NIC_VECTOR as u8, apic) {
        NETIRQ_GSI.store(gsi, SeqCst);
        if let Some(f) = ifopt(0) { f.dev.arm_intr(); }
        crate::println!("netsys: the card's line GSI {} -> the vector {:#x} onto the core APIC {} ({})",
            gsi, crate::idt::NIC_VECTOR, apic,
            if forced != 0 { "the number is set by hand" } else { "the number is derived from the slots' layout" });
    } else {
        crate::println!("netsys: the I/O APIC did not accept the GSI {} (there are {} controllers) — the line is not laid", gsi, crate::ioapic::count());
    }
}

/// The card's line that asked for itself LATER than the stack was brought up — from the tail of the user-data
/// (EC2: `talaria.netirq=1` arrives by the IMDS, that is, through an already live network).
/// The stack is not reopened: the line and the waiter's parking are applied in place.
pub fn line_late() {
    if !ready() { return; }
    if NETIRQ_GSI.load(core::sync::atomic::Ordering::SeqCst) != u32::MAX { return; }
    netirq_setup();
    if NETIRQ_GSI.load(core::sync::atomic::Ordering::SeqCst) != u32::MAX { netpark_setup(); }
}

/// The card line's handler (`idt::NIC_VECTOR`). It drives the stack: the frame for whose sake
/// the interrupt came must get into the socket's buffer IMMEDIATELY rather than
/// when the socket's owner reaches the processor. If somebody else is driving the stack right now —
/// we do not get in the way: it will take the frame.
#[unsafe(no_mangle)]
pub extern "C" fn nic_irq() {
    use core::sync::atomic::Ordering::Relaxed;
    NIC_IRQS.fetch_add(1, Relaxed);
    let cpu = crate::percpu::cpu_id().min(crate::smp::MAXCPU - 1);
    IN_NIC_IRQ[cpu].fetch_add(1, Relaxed);
    // Remove the device's interrupt status and drive the stack. `poll` takes the lock
    // BY AN ATTEMPT, so a busy stack is not a loss: the frame will be taken by the one inside.
    if let Some(f) = ifopt(0) { f.dev.ack_intr(); }
    let before = NET_SKIPS.load(Relaxed);
    poll_forced();
    if NET_SKIPS.load(Relaxed) != before {
        NIC_IRQ_BUSY.fetch_add(1, Relaxed);
        NIC_PENDING.store(true, core::sync::atomic::Ordering::SeqCst); // somebody must finish the pass
        // ...and if the holder released between our failed try and the flag, it may have
        // read the flag before we set it: try once more — the other half of the pair in
        // `NetGuard::drop` (either it sees the flag, or this try sees the lock free).
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        poll_forced();
    }
    if let Some(f) = ifopt(0) { f.dev.arm_intr(); }
    IN_NIC_IRQ[cpu].fetch_sub(1, Relaxed);
}

fn net_enter(wait: bool) -> Option<NetGuard> {
    use core::sync::atomic::Ordering::*;
    let cpu = crate::percpu::cpu_id();
    let me = cpu as i64;
    // From a tick a recursion is FORBIDDEN: the lock could have been taken by the interrupted task on
    // this same core, and "our own core" here does not mean "our own chain of calls".
    // From the card line's handler a recursion is forbidden for exactly the same reason
    // as from a tick: the lock could have been taken by THE INTERRUPTED task on this same core, and
    // "our own core" here does not mean "our own chain of calls". To enter as a second one is
    // to enter on top of a live borrow of the socket set.
    let tick = crate::console::in_tick() || IN_NIC_IRQ[cpu.min(crate::smp::MAXCPU - 1)].load(Relaxed) != 0;
    let t0 = tsc::rdtsc();
    let mut spun = false;
    let hz_diag = crate::tsc::hz(); // 0 before the calibration — then without a diagnosis
    let mut next_report = hz_diag;
    let mut reports = 0u32;
    loop {
        if !tick && NET_OWNER.load(Acquire) == me {
            NET_DEPTH.fetch_add(1, AcqRel);
            if cpu < crate::smp::MAXCPU { NET_HELD[cpu].fetch_add(1, AcqRel); }
            return Some(NetGuard(cpu as i32));
        }
        if NET_OWNER.compare_exchange(-1, me, AcqRel, Acquire).is_ok() {
            NET_DEPTH.store(1, Release);
            if cpu < crate::smp::MAXCPU { NET_HELD[cpu].fetch_add(1, AcqRel); }
            let now = tsc::rdtsc();
            NET_TAKES.fetch_add(1, Relaxed);
            if spun { NET_CONT.fetch_add(1, Relaxed); NET_WAIT_TSC.fetch_add(now.wrapping_sub(t0), Relaxed); }
            NET_SINCE.store(now, Relaxed);
            return Some(NetGuard(cpu as i32));
        }
        if !wait || tick {
            NET_SKIPS.fetch_add(1, Relaxed);
            return None;                 // a tick NEVER waits: it is the handler's body
        }
        NET_SPINS.fetch_add(1, Relaxed);
        spun = true;
        crate::paging::tlb_service(); // a neighbour in munmap waits for our acknowledgement
        core::hint::spin_loop();
        // We wait as long as necessary, but NOT SILENTLY: the stack's lock is held for microseconds
        // (the worst in the runs was 9.8 ms from close()), a second is a defect, and
        // the diagnosis must name the holder while the machine is still alive.
        let waited = tsc::rdtsc().wrapping_sub(t0);
        if hz_diag != 0 && waited > next_report {
            let hz = hz_diag;
            if reports == 0 { NET_LONGWAIT.fetch_add(1, Relaxed); }
            crate::println!("netsys: the stack's lock did not give in within {} s: core {} holds it (the depth {}, already {} ms), we are core {} task {}; we \
                keep waiting",
                waited / hz, NET_OWNER.load(Relaxed), NET_DEPTH.load(Relaxed),
                tsc::rdtsc().wrapping_sub(NET_SINCE.load(Relaxed)) / (hz / 1000).max(1), me, crate::sched::current_id());
            reports += 1;
            next_report = waited + hz * 10;
        }
    }
}
/// The waits for the stack's lock longer than a second (each one with a diagnosis into the console).
pub static NET_LONGWAIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Take the lock (we wait actively: the path is short, while falling asleep in the middle of a syscall is not allowed).
/// From a tick waiting is forbidden, so a refusal is possible there — see `net_enter`.
fn net_lock() -> Option<NetGuard> { net_enter(true) }
/// An attempt without waiting: for `poll()` — if the stack is already being driven, a second pass
/// is not needed, the work is being done.
fn net_try() -> Option<NetGuard> { net_enter(false) }

/// Hold the stack's lock for the duration of the caller's short operation.
///
/// It is needed by a `writev` over a socket: it assembles the iovecs into one buffer and sends them in ONE
/// piece, while the buffer is per-processor. The stack's lock at the same time forbids preemption on
/// this core (see `holds_lock`), so between the filling of the buffer and the sending
/// a neighbouring task will not squeeze in here.
pub fn hold<R>(f: impl FnOnce() -> R) -> R {
    let _g = net_enter(true);
    f()
}

/// How many times the stack's lock was waited for and how many times an observation backed off (kstat).
pub fn lock_spins() -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (NET_SPINS.load(Relaxed), NET_SKIPS.load(Relaxed))
}

/// THE CONNECTIONS' TRACE (`net: connect/close ...`) is only by `talaria.nettrace=1`.
///
/// Previously it was on always, and the line `net: close` was printed UNDER THE STACK'S
/// LOCK: printing into the serial goes with IF=0 and costs a VMEXIT per byte, while
/// the workload at the same moment writes its own "end connection" into the same console.
/// The histogram of the holdings named that precisely: all the holdings of the lock longer than
/// a millisecond (up to 8.7 ms) were from close(), and in none of them did the pass of the stack itself
/// take even a millisecond. The datapath must not depend on whether the trace
/// is on — so the printing in close() has also been moved OUTSIDE the lock.
static NET_TRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
fn trace_on() -> bool { NET_TRACE.load(core::sync::atomic::Ordering::Relaxed) }
fn ep_parts(ep: Option<IpEndpoint>) -> ([u8; 4], u16) {
    match ep { Some(e) => match e.addr { IpAddress::Ipv4(a) => (a.octets(), e.port) }, None => ([0; 4], 0) }
}

#[derive(Clone, Copy)]
struct Udp { used: bool, nonblock: bool, bound: bool, armed: bool, remote: Option<IpEndpoint>, ifx: u8, port: u16, tid: u8, h: Option<SocketHandle> }
const UDP0: Udp = Udp { used: false, nonblock: false, bound: false, armed: false, remote: None, ifx: 0, port: 0, tid: 255, h: None };

/// What the operator sees about an interface (`ip`, `status`) and what they control.
#[derive(Clone, Copy)]
pub struct IfInfo {
    pub name: [u8; 8],
    pub nlen: usize,
    pub drv: &'static str,
    pub mac: [u8; 6],
    pub up: bool,
    pub dhcp: bool,
    pub ip: [u8; 4],
    pub prefix: u8,
    pub gw: [u8; 4],
}
impl IfInfo {
    pub fn name_str(&self) -> &[u8] { &self.name[..self.nlen] }
}

struct Iface {
    dev: SmolDev,
    iface: Interface,
    sockets: SocketSet<'static>,
    dh: Option<SocketHandle>,   // this interface's DHCP client
    ke: Option<SocketHandle>,   // §7.4: the kernel echo
    ks: Option<SocketHandle>,   // A3: the sink
    nt: Option<SocketHandle>,   // the wall clock's SNTP client (`talaria.ntp=`, the first interface)
    info: IfInfo,
}
static mut IFS: [Option<Iface>; MAXIF] = [None, None, None, None];
static mut NIF: usize = 0;
static mut TT: [Tcp; NS] = [TCP0; NS];
static mut UU: [Udp; NU] = [UDP0; NU];
const STORE_N: usize = NS + NU + 2;
const STORE0: [SocketStorage; STORE_N] = [SocketStorage::EMPTY; STORE_N];
static mut STORE: [[SocketStorage; STORE_N]; MAXIF] = [STORE0, STORE0, STORE0, STORE0];
static mut RXB: [[u8; RXB_SZ]; NS] = [[0; RXB_SZ]; NS];
static mut TXB: [[u8; TXB_SZ]; NS] = [[0; TXB_SZ]; NS];
static mut URXM: [[udp::PacketMetadata; UPKT]; NU] = [[udp::PacketMetadata::EMPTY; UPKT]; NU];
static mut UTXM: [[udp::PacketMetadata; UPKT]; NU] = [[udp::PacketMetadata::EMPTY; UPKT]; NU];
static mut URXP: [[u8; UBUF]; NU] = [[0; UBUF]; NU];
static mut UTXP: [[u8; UBUF]; NU] = [[0; UBUF]; NU];
static mut HZ: u64 = 1;
fn rxb(i: usize) -> &'static mut [u8] { unsafe { &mut *addr_of_mut!(RXB[i]) } }
fn txb(i: usize) -> &'static mut [u8] { unsafe { &mut *addr_of_mut!(TXB[i]) } }
fn tt() -> &'static mut [Tcp; NS] { unsafe { &mut *addr_of_mut!(TT) } }
fn uu() -> &'static mut [Udp; NU] { unsafe { &mut *addr_of_mut!(UU) } }

/// How many interfaces have been created.
pub fn nifs() -> usize { unsafe { NIF } }
fn ifopt(i: usize) -> Option<&'static mut Iface> {
    unsafe { if i < MAXIF { (*addr_of_mut!(IFS[i])).as_mut() } else { None } }
}
fn ifr(i: usize) -> &'static mut Iface { ifopt(i).expect("iface") }
/// The summary for the interface i (for the console and netcfg).
pub fn if_info(i: usize) -> Option<IfInfo> { ifopt(i).map(|f| f.info) }
/// An interface's index by name (`net0`).
pub fn if_by_name(name: &str) -> Option<usize> {
    for i in 0..unsafe { NIF } {
        if let Some(f) = ifopt(i) { if f.info.name_str() == name.as_bytes() { return Some(i); } }
    }
    None
}
/// An interface's index by MAC address (`52-54-00-12-34-56`). The order in which the cards are
/// enumerated is set by the hardware (the window's address/the PCI function), it is stable, but it does NOT
/// coincide with the order in which a human enumerated the cards. A rule by MAC
/// ties an address to A PARTICULAR card and does not depend on the enumeration.
pub fn if_by_mac(mac: [u8; 6]) -> Option<usize> {
    for i in 0..unsafe { NIF } {
        if let Some(f) = ifopt(i) { if f.info.mac == mac { return Some(i); } }
    }
    None
}
/// The first brought-up interface — it is used by the paths to which it does not matter which one.
pub fn any_up() -> Option<usize> {
    for i in 0..unsafe { NIF } { if ifopt(i).is_some_and(|f| f.info.up) { return Some(i); } }
    None
}

// ── the configuration of THE FIRST interface (the cmdline talaria.ip=/gw=/dns=) ────
// The other interfaces are configured through netcfg (`talaria.net=`, `ip addr add`).
static mut CFG_IP: [u8; 4] = [10, 0, 0, 2];
static mut CFG_PREFIX: u8 = 24;
static mut CFG_GW: [u8; 4] = [10, 0, 0, 1];
static mut CFG_DNS: [u8; 4] = [10, 0, 0, 1];
static mut CFG_DHCP: bool = false; // talaria.ip=dhcp: the address/gateway/DNS from DHCP (a VPC/slirp), the static one is a fallback
static mut CFG_DHCP_WAIT: bool = false; // true while the lease has not been received (init waits, poll resets it)
static mut CFG_IMDS: bool = false;
/// The boot DEPENDS on the DHCP lease (the user-data/IMDS are read only with a VPC address):
/// wait for it longer than on the stand, and do not pass the static one off as a success silently.
static mut CFG_DHCP_VITAL: bool = false;
// §7.4/§4.9: a UDP ECHO RIGHT IN THE KERNEL (without syscalls and ring 3) on a given port —
// the reference of "how much our path costs without a user process"; with talaria.kts=1
// the answer's first 8 bytes are the kernel's wall clock in nanoseconds (a one-way measurement, §4.9).
static mut CFG_KECHO: u16 = 0;
static mut CFG_KTS: bool = false;
// A3: talaria.ksink=port — a "black hole" for measuring the RECEIVE path. Without it a background
// flow to an unoccupied port makes the stack build an ICMP "the port is unreachable" for EVERY packet,
// and rxbench would measure the speed of those answers rather than of the receive path.
static mut CFG_KSINK: u16 = 0;
// talaria.ntp=aws|a.b.c.d[,poll s]: the wall clock's reference over the network (`ntp.rs`).
static mut CFG_NTP: Option<([u8; 4], u32)> = None;
/// The SNTP client's poll: the burst in flight, when the next one is due, what it fed.
/// `boot`: the boot is still waiting for the first sample — it sets the base instead of stepping.
struct NtpState { b: crate::ntp::Burst, next: u64, last_tx: u64, burst_t0: u64, fed: u32, silent: u32, boot: bool }
static mut NTP: NtpState = NtpState { b: crate::ntp::Burst::new(), next: 0, last_tx: 0, burst_t0: 0, fed: 0, silent: 0, boot: false };
/// The samples fed to the servo so far (the boot's wait and the gate read it).
pub fn ntp_fed() -> u32 { unsafe { NTP.fed } }
// §7.5: when idle, SLEEP on the receive ring (MONITOR/MWAIT) rather than spin a poll().
static mut CFG_NETIDLE: bool = false;
pub fn configure(ip: [u8; 4], prefix: u8, gw: [u8; 4], dns: [u8; 4]) {
    unsafe { CFG_IP = ip; CFG_PREFIX = prefix; CFG_GW = gw; CFG_DNS = dns; }
}
/// The address/gateway of THE MAIN interface (the one the default route goes
/// through). Previously that was the only configuration in the system.
pub fn cfg_ip() -> [u8; 4] {
    match crate::netcfg::default_ifx().and_then(if_info) { Some(f) if f.ip != [0; 4] => f.ip, _ => unsafe { CFG_IP } }
}
pub fn cfg_gw() -> [u8; 4] {
    match crate::netcfg::default_ifx().and_then(if_info) { Some(f) if f.gw != [0; 4] => f.gw, _ => unsafe { CFG_GW } }
}
pub fn cfg_dns() -> [u8; 4] { unsafe { CFG_DNS } }
pub fn set_dns(d: [u8; 4]) { unsafe { CFG_DNS = d; } crate::fs::vf_init_net(d); }
/// Parse "A.B.C.D" (optionally "/P"). None at an error.
pub fn parse_ip(s: &str) -> Option<([u8; 4], u8)> {
    let (addr, pfx) = match s.split_once('/') { Some((a, p)) => (a, p.parse::<u8>().ok()?), None => (s, 24) };
    let mut ip = [0u8; 4]; let mut n = 0;
    for part in addr.split('.') { if n >= 4 { return None; } ip[n] = part.parse::<u8>().ok()?; n += 1; }
    if n != 4 { return None; }
    Some((ip, pfx))
}
/// Apply talaria.ip=/gw=/dns= from the cmdline (if they are set).
pub fn configure_from_cmdline(cmdline: &str) {
    let (mut ip, mut pfx, mut gw, mut dns) = unsafe { (CFG_IP, CFG_PREFIX, CFG_GW, CFG_DNS) };
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("talaria.ip=") {
            if v == "dhcp" { unsafe { CFG_DHCP = true; CFG_DHCP_WAIT = true; } }
            else if let Some((a, p)) = parse_ip(v) { ip = a; pfx = p; }
        }
        else if let Some(v) = tok.strip_prefix("talaria.gw=") { if let Some((a, _)) = parse_ip(v) { gw = a; } }
        else if let Some(v) = tok.strip_prefix("talaria.dns=") { if let Some((a, _)) = parse_ip(v) { dns = a; } }
        else if let Some(v) = tok.strip_prefix("talaria.netirq=") {
            // `1` — derive the line from the mmio slot; `all` — lay ALL the lines of both
            // controllers (a bisect: which one fires at all); a number — take it.
            NETIRQ_WANTED.store(true, core::sync::atomic::Ordering::SeqCst);
            // `0` is A PROHIBITION rather than "switch it on with a zero". Previously any value
            // switched the line on, and there was nothing to switch it off with.
            NETIRQ_MODE.store(if v == "0" { 2 } else { 1 }, core::sync::atomic::Ordering::SeqCst);
            if v == "all" { NETIRQ_ALL.store(true, core::sync::atomic::Ordering::SeqCst); }
            else if let Some((a, b)) = v.split_once('-') {
                // The range `a-b`: a bisect over the lines when the layout is unknown.
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    NETIRQ_FROM.store(a, core::sync::atomic::Ordering::SeqCst);
                    NETIRQ_TO.store(b, core::sync::atomic::Ordering::SeqCst);
                }
            }
            else if let Ok(g) = v.parse::<u32>() { if g > 1 { NETIRQ_FORCE.store(g, core::sync::atomic::Ordering::SeqCst); } }
        }
        else if tok == "talaria.imds=1" { unsafe { CFG_IMDS = true; CFG_DHCP_VITAL = true; } }
        else if tok == "talaria.userdata=1" || tok == "talaria.userdata" { unsafe { CFG_DHCP_VITAL = true; } }
        else if let Some(v) = tok.strip_prefix("talaria.kecho=") { if let Ok(p) = v.parse::<u16>() { unsafe { CFG_KECHO = p; } } }
        else if let Some(v) = tok.strip_prefix("talaria.ksink=") { if let Ok(p) = v.parse::<u16>() { unsafe { CFG_KSINK = p; } } }
        else if tok == "talaria.kts=1" { unsafe { CFG_KTS = true; } }
        else if let Some(v) = tok.strip_prefix("talaria.ntp=") {
            match crate::ntp::parse_knob(v) {
                Some(c) => unsafe { CFG_NTP = Some(c); },
                None => crate::println!("netsys: talaria.ntp={} is not aws or a.b.c.d[,poll s] — no reference", v),
            }
        }
        else if tok == "talaria.netidle=1" { unsafe { CFG_NETIDLE = true; } }
        else if tok == "talaria.netsolo=1" {
            // A PASS OF THE STACK IS ONE PER MACHINE: the non-polling waiters park.
            // A separate knob rather than a side effect of `netpark=`: the parking of
            // THE POLLING one requires the card's line (otherwise there is nobody to drive the stack),
            // while the parking of THE REST requires nothing — the stack will be driven for them by
            // the polling one. Previously that switched itself on by the number of waiters and
            // cost MariaDB 29 times (see `wait_frame_w`).
            NETSOLO.store(true, core::sync::atomic::Ordering::SeqCst);
        }
        else if let Some(v) = tok.strip_prefix("talaria.netmix=") {
            // A waiter whose set is NOT only sockets parks too (`wait_frame_w`), with this
            // ceiling in nanoseconds; implies `netsolo`. 0 — off (the default: the spin).
            if let Ok(ns) = v.parse::<u64>() { NETMIX_NS.store(ns, core::sync::atomic::Ordering::SeqCst); }
        }
        else if let Some(v) = tok.strip_prefix("talaria.netpark=") {
            // The ceiling of a waiter's parking in nanoseconds; 0 — switch it off (the former
            // polling). An explicitly set value cancels the default, which
            // is switched on together with the card's line.
            if let Ok(ns) = v.parse::<u64>() {
                NETPARK_NS.store(ns, core::sync::atomic::Ordering::SeqCst);
                NETPARK_SET.store(true, core::sync::atomic::Ordering::SeqCst);
            }
        }
    }
    configure(ip, pfx, gw, dns);
    crate::netcfg::configure_from_cmdline(cmdline);
}

fn nowi(hz: u64) -> smoltcp::time::Instant {
    let t = tsc::rdtsc();
    smoltcp::time::Instant::from_micros(((t / hz) * 1_000_000 + (t % hz) * 1_000_000 / hz) as i64)
}

/// Create an interface on top of a card. It returns its index.
///
/// The first interface gets its configuration from the cmdline (`talaria.ip=`), the rest
/// come up WITHOUT an address: an address is given to them by a `talaria.net=` rule or by the operator
/// with the command `ip addr add`. Handing addresses out to the second cards silently is not allowed — that is
/// the fastest way to get two interfaces in one subnet and undebuggable
/// routing.
pub fn add_iface(dev: &'static mut dyn crate::netdev::NetDev, mac: [u8; 6], drv: &'static str, hz: u64) -> Option<usize> {
    unsafe {
        if NIF >= MAXIF { crate::println!("netsys: there are already {} interfaces — the card is not connected", MAXIF); return None; }
        let idx = NIF;
        HZ = hz;
        let mut sdev = SmolDev::new_n(dev, idx);
        let cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        let iface = Interface::new(cfg, &mut sdev, nowi(hz));
        let st: &'static mut [SocketStorage] = core::slice::from_raw_parts_mut(addr_of_mut!(STORE[idx]) as *mut SocketStorage, STORE_N);
        let sockets = SocketSet::new(st);
        let mut name = [0u8; 8];
        name[..3].copy_from_slice(b"net"); name[3] = b'0' + idx as u8;
        let first = idx == 0;
        let info = IfInfo {
            name, nlen: 4, drv, mac, up: true,
            dhcp: if first { CFG_DHCP } else { false },
            ip: if first && !CFG_DHCP { CFG_IP } else { [0; 4] },
            prefix: if first && !CFG_DHCP { CFG_PREFIX } else { 0 },
            gw: if first && !CFG_DHCP { CFG_GW } else { [0; 4] },
        };
        IFS[idx] = Some(Iface { dev: sdev, iface, sockets, dh: None, ke: None, ks: None, nt: None, info });
        NIF = idx + 1;
        // THE SECOND CARD CANCELS THE WATCHING OF THE RING: the silence of the first says nothing
        // about the second, while a pass of the stack goes over ALL the interfaces at once.
        // Cards are added AFTER the stack is brought up too (the management network, filed),
        // so the window is removed here rather than only at the creation.
        if NIF > 1 && RING_WATCH.load(core::sync::atomic::Ordering::SeqCst) {
            RING_WATCH.store(false, core::sync::atomic::Ordering::SeqCst);
            crate::println!("netsys: there is more than one interface — an idle pass is throttled by time again");
        }
        // apply the starting configuration to smoltcp
        if info.ip != [0; 4] { apply_addr(idx); }
        // The route from the cmdline was set by THE OPERATOR rather than by DHCP: the mark matters because
        // a lease tears down only its own routes.
        if info.gw != [0; 4] { crate::netcfg::add_route_f([0, 0, 0, 0], 0, info.gw, idx, 0, false); }
        if info.dhcp { start_dhcp(idx); }
        crate::println!("netsys: the interface net{} on the card '{}' mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            idx, drv, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
        Some(idx)
    }
}

/// Carry the interface's address over into smoltcp (after an `ip addr add`/DHCP).
pub fn apply_addr(idx: usize) {
    let _g = match net_lock() { Some(g) => g, None => return Default::default() };
    let f = match ifopt(idx) { Some(f) => f, None => return };
    let (ip, pfx) = (f.info.ip, f.info.prefix);
    f.iface.update_ip_addrs(|a| {
        a.clear();
        if ip != [0; 4] { let _ = a.push(IpCidr::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), pfx)); }
    });
    crate::netcfg::apply_iface(idx);
}
/// Set an interface's address (the operator or DHCP).
pub fn if_set_addr(idx: usize, ip: [u8; 4], prefix: u8) {
    if let Some(f) = ifopt(idx) { f.info.ip = ip; f.info.prefix = prefix; }
    apply_addr(idx);
}
pub fn if_set_gw(idx: usize, gw: [u8; 4]) { if let Some(f) = ifopt(idx) { f.info.gw = gw; } }
/// Bring an interface up/down. A downed one neither sends nor receives: its
/// sockets are reset, the routes through it stop being chosen.
pub fn if_set_up(idx: usize, up: bool) {
    let _g = match net_lock() { Some(g) => g, None => return Default::default() };
    if let Some(f) = ifopt(idx) { f.info.up = up; }
    if !up {
        for i in 0..NS { if tt()[i].ifx as usize == idx && tt()[i].k != K::Free { abort_slot(i); } }
    }
}
pub fn if_up(idx: usize) -> bool { ifopt(idx).is_some_and(|f| f.info.up) }
/// Switch DHCP on/off on an interface.
pub fn if_set_dhcp(idx: usize, on: bool) {
    let f = match ifopt(idx) { Some(f) => f, None => return };
    if on && f.dh.is_none() { f.info.dhcp = true; start_dhcp(idx); }
    else if !on {
        if let Some(h) = f.dh.take() { f.sockets.remove(h); }
        f.info.dhcp = false;
    }
}
fn start_dhcp(idx: usize) {
    let f = match ifopt(idx) { Some(f) => f, None => return };
    if f.dh.is_some() { return; }
    let mut d = dhcpv4::Socket::new();
    // A REPEAT OF THE DISCOVER ONCE EVERY 2 S rather than every 10. With smoltcp by default a DISCOVER
    // is repeated after 10 s, while the wait for the lease at the boot was 8 s: ONE
    // lost DISCOVER/OFFER (on a fresh EC2 instance the card's first frame
    // is lost easily) — and the stack "by the timeout" sat down on the static 10.0.0.2,
    // the IMDS was unreachable, the user-data were empty, the machine came up IN A FOREIGN ROLE
    // (the suite's generator on EC2 started PostgreSQL) and burnt the stand for hours.
    let mut rc = dhcpv4::RetryConfig::default();
    rc.discover_timeout = smoltcp::time::Duration::from_secs(2);
    rc.initial_request_timeout = smoltcp::time::Duration::from_secs(2);
    d.set_retry_config(rc);
    f.dh = Some(f.sockets.add(d));
}
/// Request the lease again (the operator: `dhcp net0 renew`).
pub fn dhcp_renew(idx: usize) {
    let _g = match net_lock() { Some(g) => g, None => return Default::default() };
    let f = match ifopt(idx) { Some(f) => f, None => return };
    if let Some(h) = f.dh.take() { f.sockets.remove(h); }
    f.info.dhcp = true;
    start_dhcp(idx);
}
/// The counters of an interface's card (the calls to the device) — for `ip -s`.
pub fn if_doorbells(idx: usize) -> Option<u64> { ifopt(idx).and_then(|f| f.dev.db_writes()) }
/// The frames/bytes received on an interface: it separates "the card is silent" from "the address is wrong".
pub fn if_rx(idx: usize) -> (u64, u64) { ifopt(idx).map(|f| f.dev.rx_counters()).unwrap_or((0, 0)) }
/// Give `netcfg` access to THIS interface's routing table. smoltcp's internals
/// do not stick out — only this window, and only for the routes.
pub fn with_routes<F: FnOnce(&mut smoltcp::iface::Routes)>(idx: usize, f: F) {
    if let Some(i) = ifopt(idx) { f(i.iface.routes_mut()); }
}

pub fn init(dev: &'static mut dyn crate::netdev::NetDev, mac: [u8; 6], hz: u64) { init_named(dev, mac, "nic", hz) }
/// The same, but with THE DRIVER'S NAME: in `ip` the operator must see what card is under
/// the interface — "net0 without an address" and "net0 on a card that does not exist" are cured
/// differently.
pub fn init_named(dev: &'static mut dyn crate::netdev::NetDev, mac: [u8; 6], drv: &'static str, hz: u64) {
    // §0.1: the stack could have been brought up EARLIER (the early phase of the user-data) — then a repeated
    // call from a mode's branch must be a no-op, otherwise a second initialisation of the card
    // will overwrite the rings of a working datapath.
    if ready() { let _ = dev; return; }
    let idx = match add_iface(dev, mac, drv, hz) { Some(i) => i, None => return };
    // §7.5: sleeping when idle is possible only with a working LAPIC.
    if unsafe { CFG_NETIDLE } { crate::smp::lapic_enable(); crate::println!("netsys: idling = a sleep on the receive ring (talaria.netidle=1, the ceiling is \
        200 us)"); }
    // The card's line is laid HERE rather than in a mode's branch: there are four branches, and
    // previously the call stood in one of them (the EC2 path), that is, on an ordinary boot it did not
    // fire at all.
    netirq_setup();
    netpark_setup();
    ring_watch_setup();
    // §7.4: the kernel echo occupies THE LAST UDP slot (ring 3 will not get it)
    let kp = unsafe { CFG_KECHO };
    if kp != 0 {
        let f = ifr(idx);
        let h = f.sockets.add(new_udp(NU - 1));
        if f.sockets.get_mut::<udp::Socket>(h).bind(kp).is_ok() {
            uu()[NU - 1] = Udp { used: true, bound: true, ifx: idx as u8, port: kp, h: Some(h), ..UDP0 };
            f.ke = Some(h);
            crate::println!("netsys: the kernel echo UDP :{}{} (the answer comes from poll(), without ring 3)",
                kp, if unsafe { CFG_KTS } { ", a wall-clock stamp in the first 8 bytes" } else { "" });
        }
    }
    // A3: the sink occupies THE SECOND-TO-LAST UDP slot (the last one is the kernel echo's).
    let sp = unsafe { CFG_KSINK };
    if sp != 0 && NU >= 2 {
        let f = ifr(idx);
        let h = f.sockets.add(new_udp(NU - 2));
        if f.sockets.get_mut::<udp::Socket>(h).bind(sp).is_ok() {
            uu()[NU - 2] = Udp { used: true, bound: true, ifx: idx as u8, port: sp, h: Some(h), ..UDP0 };
            f.ks = Some(h);
            crate::println!("netsys: the sink UDP :{} (accept and throw away — a measurement of the receive path)", sp);
        }
    }
    // The wall clock's SNTP client occupies THE THIRD-FROM-LAST UDP slot (the sink's and the
    // echo's are the last two); any free local port.
    if let Some((srv, iv)) = unsafe { CFG_NTP } {
        let f = ifr(idx);
        let h = f.sockets.add(new_udp(NU - 3));
        if NU >= 3 && f.sockets.get_mut::<udp::Socket>(h).bind(10123).is_ok() {
            uu()[NU - 3] = Udp { used: true, bound: true, ifx: idx as u8, port: 10123, h: Some(h), ..UDP0 };
            f.nt = Some(h);
            crate::wallclock::set_net_ref();
            crate::println!("netsys: the wall clock's reference is SNTP to {}.{}.{}.{} every {} s (a burst of {}, the shortest round trip is the sample)",
                srv[0], srv[1], srv[2], srv[3], iv, crate::ntp::BURST);
        }
    }
    if unsafe { CFG_DHCP } {
        // A synchronous wait for the lease (DISCOVER→ACK; with slirp/a VPC it is single ms,
        // the DISCOVER is repeated once every 2 s, see `start_dhcp`). On the stand we wait 8 s
        // and sit down on the static one; where a boot without a lease is meaningless (the user-data,
        // the IMDS) it is 60 s, and every 10 s we say that we are waiting: a silent fallback onto
        // 10.0.0.2 in a VPC is a machine in a foreign role.
        let vital = unsafe { CFG_DHCP_VITAL };
        let budget = if vital { 60 } else { 8 };
        let t0 = tsc::rdtsc();
        let deadline = t0 + budget * hz;
        let mut got = false;
        let mut next_say = t0 + 10 * hz;
        while tsc::rdtsc() < deadline {
            poll();
            if unsafe { !CFG_DHCP_WAIT } { got = true; break; }
            let now = tsc::rdtsc();
            if now > next_say {
                crate::println!("netsys: dhcp: there has been no lease for {} s already (the budget is {} s), the DISCOVER is being repeated",
                    now.wrapping_sub(t0) / hz.max(1), budget);
                next_say = now + 10 * hz;
            }
            core::hint::spin_loop();
        }
        if !got {
            let (ip, pfx, gw) = unsafe { (CFG_IP, CFG_PREFIX, CFG_GW) };
            if_set_addr(idx, ip, pfx);
            if_set_gw(idx, gw);
            crate::netcfg::add_route_f([0, 0, 0, 0], 0, gw, idx, 0, false);
            crate::println!("netsys: a dhcp TIMEOUT after {} s — a fallback onto the static one{}", budget,
                if vital { " (the user-data/IMDS will be UNREACHABLE — the machine will come up without a role)" } else { "" });
        }
    }
    let (ip, gw, dns) = (cfg_ip(), cfg_gw(), unsafe { CFG_DNS });
    let pfx = if_info(idx).map_or(24, |f| f.prefix);
    crate::println!("netsys: ip={}.{}.{}.{}/{} gw={}.{}.{}.{} dns={}.{}.{}.{} tcp={} udp={}{}",
        ip[0], ip[1], ip[2], ip[3], pfx, gw[0], gw[1], gw[2], gw[3],
        dns[0], dns[1], dns[2], dns[3], NS, NU, if unsafe { CFG_DHCP } { " (dhcp)" } else { "" });
    if unsafe { CFG_IMDS } { crate::imds::fetch(); } // talaria.imds=1 (the EC2 metadata, §3.3)
    // The first sample before the workload starts: its first timestamps are the reference's,
    // not the firmware's whole seconds. A bounded wait — no answer is said and the poll goes on.
    if ifr(idx).nt.is_some() {
        let t0 = tsc::rdtsc();
        unsafe { NTP.boot = true; }
        while ntp_fed() == 0 && tsc::rdtsc().wrapping_sub(t0) < 3 * hz { poll(); core::hint::spin_loop(); }
        unsafe { NTP.boot = false; }
        if ntp_fed() == 0 { crate::println!("netsys: ntp: no sample in 3 s — the clock keeps its base, the poll goes on"); }
    }
}
pub fn ready() -> bool { unsafe { NIF > 0 } }
/// §0.1: take the stack down after the early phase of the user-data, PRESERVING the received lease as
/// a static one. A mode's branch will open the card anew.
pub fn teardown_keep_lease() {
    let _g = match net_lock() { Some(g) => g, None => return Default::default() };
    unsafe {
        let ip = cfg_ip(); let gw = cfg_gw();
        CFG_IP = ip; CFG_GW = gw;
        if let Some(f) = if_info(0) { CFG_PREFIX = if f.prefix != 0 { f.prefix } else { CFG_PREFIX }; }
        for i in 0..MAXIF { IFS[i] = None; }
        NIF = 0;
        for i in 0..NS { TT[i] = TCP0; }
        for i in 0..NU { UU[i] = UDP0; }
        // The sockets' storage is a static: without a cleanup a second SocketSet sees the slots
        // occupied and smoltcp panics with "adding a socket to a full SocketSet".
        for k in 0..MAXIF { for s in (*addr_of_mut!(STORE[k])).iter_mut() { *s = SocketStorage::EMPTY; } }
        crate::netcfg::clear();
        CFG_DHCP = false;      // the lease is already in CFG_IP/CFG_GW/CFG_DNS
        CFG_DHCP_WAIT = false;
    }
}

fn new_udp(i: usize) -> udp::Socket<'static> {
    unsafe {
        udp::Socket::new(
            udp::PacketBuffer::new(&mut *addr_of_mut!(URXM[i]) as &mut [_], &mut *addr_of_mut!(URXP[i]) as &mut [u8]),
            udp::PacketBuffer::new(&mut *addr_of_mut!(UTXM[i]) as &mut [_], &mut *addr_of_mut!(UTXP[i]) as &mut [u8]))
    }
}
fn new_tcp(i: usize) -> tcp::Socket<'static> {
    let mut s = tcp::Socket::new(tcp::SocketBuffer::new(rxb(i)), tcp::SocketBuffer::new(txb(i)));
    // THE ACK IS PIGGYBACKED ON THE ANSWER rather than going as a separate packet.
    //
    // Previously there stood a `None` — "acknowledge at once, delay nothing", and by
    // the name that looks like a setting for a low latency. In fact for a
    // "request-answer" server it MAKES IT WORSE: TWO frames instead of one left per request
    // (first a bare ACK, then the answer), that is, twice
    // as many passes of the stack and twice as many calls to the card. Under a hypervisor
    // a call is a VMEXIT of several microseconds; the interface's counter honestly
    // showed a tx exactly twice as large as the rx.
    //
    // A millisecond is the waiting's ceiling rather than a latency: the application's answer comes
    // within tens of microseconds and carries the ACK away with it. If the application stays silent
    // longer, the ACK leaves by itself. The same value in essence stands in smoltcp by
    // default (there it is 10 ms), so we are still ten times more impatient.
    s.set_ack_delay(Some(smoltcp::time::Duration::from_millis(1)));
    s
}

/// Bind a TCP slot to an interface (create a socket in its set).
fn attach_tcp(slot: usize, ifx: usize) -> bool {
    if ifopt(ifx).is_none() { return false; }
    detach_tcp(slot);
    let f = ifr(ifx);
    let h = f.sockets.add(new_tcp(slot));
    tt()[slot].ifx = ifx as u8;
    tt()[slot].h = Some(h);
    true
}
fn detach_tcp(slot: usize) {
    let t = tt()[slot];
    if let Some(h) = t.h {
        if let Some(f) = ifopt(t.ifx as usize) { f.sockets.remove(h); }
        tt()[slot].h = None;
    }
}
fn attach_udp(slot: usize, ifx: usize) -> bool {
    if ifopt(ifx).is_none() { return false; }
    detach_udp(slot);
    let f = ifr(ifx);
    let h = f.sockets.add(new_udp(slot));
    uu()[slot].ifx = ifx as u8;
    uu()[slot].h = Some(h);
    true
}
fn detach_udp(slot: usize) {
    let u = uu()[slot];
    if let Some(h) = u.h {
        if let Some(f) = ifopt(u.ifx as usize) { f.sockets.remove(h); }
        uu()[slot].h = None;
    }
}
fn abort_slot(i: usize) {
    if tt()[i].h.is_some() { s(i).abort(); }
    detach_tcp(i);
    tt()[i] = TCP0; note_slot_free();
}

/// Drive the stack (the cards' rx/tx + the TCP timers) over ALL the brought-up interfaces.
/// `talaria.pollgap=<ns>` — do not drive the stack more often than once every that much, IF
/// the previous pass brought nothing. 0 — as it was.
///
/// What for. A waiter in a `read`/`epoll_wait` drives the stack at every iteration of its
/// loop, and session 21's measurement showed the price: with mongod there were 352-424 PASSES OF THE STACK PER
/// ONE turnaround of a request (44.6 million passes over 126 thousand turnarounds), while the stack's lock
/// is one per machine — hence 192-284 million waits for the lock over a run. The frame meanwhile
/// comes ONCE: all the other passes are idle.
///
/// Only FRUITLESS spinning is throttled: if at least one frame has arrived since the previous pass,
/// the pass goes immediately. So a flow under load is not
/// slowed down, while an idling waiter stops fighting for the lock.
/// The default of 10 us was chosen by a measurement on PostgreSQL (the ceiling 19.2 -> 25.9 thousand
/// req/s; at 20 us it is already worse — 24.6, and the p50 grows from 65 to 87). It was checked that
/// this does not harm the rest: mongodb's p99 250 -> 247, redis and nginx are within
/// the stand's spread. 0 — as it was.
static POLLGAP_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(10_000);
static LAST_POLL_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static LAST_POLL_RX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many passes of the stack were not made (idle ones throttled by `pollgap`).
pub static POLL_SKIPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// ── ASK THE RING RATHER THAN THE CLOCK ──────────────────────────────────────
///
/// A threshold by time answers the question "how long ago did I look", while an answer to
/// a different one is needed: "has the card brought anything at all". The difference is visible in the price: by
/// time one has to leave the gap SMALL (10 us — more is already harmful,
/// measured), because a real frame hides behind it; while if one asks the ring
/// itself, there is no need to wait at all — the pass is made exactly when there is something
/// to parse.
///
/// The ring can be asked WITHOUT the lock and without an exit into the host: the word of the receive
/// `used` ring is written by the device's DMA (for ENA it is the completion queue's word), that is
/// an ordinary line of memory. The counter `l1::rx_seen()`, which stood here before,
/// could not answer that question by construction: it moves only AFTER
/// a pass, that is, it tells "we have already parsed it" from "we have not looked yet" rather than
/// "there is something new" from "there is nothing new".
///
/// THE WORD'S ADDRESS IS ASKED EVERY TIME rather than cached. With virtio that is the header
/// of the used ring — the address is constant, and a cache would be correct. But with ENA it is the descriptor
/// of the CURRENT HEAD of the completion queue, and the address MOVES together with the head:
/// a cached pointer after the very first turn would look into a slot that
/// new frames no longer touch — that is, on EC2 the sign "the card is silent" would become
/// always true, and every frame would wait for the timers' debt. It was found
/// by a readiness check for EC2, before the run itself.
static RING_WATCH: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The value of the ring's word at the moment of the LAST pass that took place.
static RING_LAST: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// A pass for the sake of the stack's own timers (retransmits, delayed ACKs,
/// TIME-WAIT) is made not less often than this, even if the card is silent.
///
/// A HUNDRED MICROSECONDS IS A MEASUREMENT rather than a taste. With a millisecond the p50 stayed the former one, while
/// the tail got worse by exactly it (mongodb's p99 reached 996 us — that is, the very
/// ceiling). With a hundred microseconds the p50 became BETTER at all the measured
/// points (redis 34.3 -> 27.2 us, nginx 37.4 -> 33.5, mongodb 74.0 -> 59.2), while
/// only 0.2 % of the passes stopped being cancelled: almost all of them are idle anyway.
/// `talaria.pollhard=<ns>`; 0 — go back to the former threshold by time.
static POLL_HARD_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(100_000);
/// HOW MANY TIMES THE RING LIED: the word did not change, while the pass did bring
/// frames. The check is needed because "the word of the receive ring" is a contract WITH
/// THE DRIVER, and we have two drivers (virtio and ENA), and with the second one it is the word of the completion
/// queue. If the contract is not fulfilled somewhere, the sign of the card's silence
/// becomes false, and a frame waits for the timers' debt instead of a delivery. A zero in this
/// counter is a proof that the sign is correct on this hardware; a non-zero one is a direct
/// indication to switch the watching off (`talaria.pollhard=0`) and to investigate.
pub static RING_LIED: AtomicU64 = AtomicU64::new(0);
/// How many passes of the stack took place only by the debt to the timers.
pub static POLL_HARD_RUNS: AtomicU64 = AtomicU64::new(0);

/// The current value of the receive ring's word (None — the window is not created: there are
/// several cards, the driver does not give the word or the stack is not up yet).
#[inline]
fn ring_word() -> Option<u32> { ring_watch_now().1 }
/// The address and the value of the receive ring's word RIGHT NOW. The tuple's first element is
/// the sign "there is nothing to compare with" (there is no window), the second is the value itself.
#[inline]
fn ring_watch_now() -> (Option<()>, Option<u32>) {
    if !RING_WATCH.load(core::sync::atomic::Ordering::Relaxed) { return (None, None); }
    let Some(f) = ifopt(0) else { return (None, None) };
    match f.dev.idle_watch() {
        Some((a, v)) => {
            RING_ADDR_NOW.store(crate::mem::vaddr(a), core::sync::atomic::Ordering::Relaxed);
            (None, Some(v))
        }
        None => (None, None),
    }
}
/// The address of the word read by the last call of `ring_watch_now`, and the address
/// remembered at the previous pass: comparing the values makes sense only when
/// they COINCIDE (see the analysis in `poll_gated`).
/// `talaria.ringhonest=0` — go back to the former sign "the card is silent"
/// (a comparison of the PAIR "the address+the value" of the ring's word). BY DEFAULT THE HONEST
/// sign IS ON — the phase bit of the CQ's head, the very one by which the driver itself
/// takes a frame.
///
/// THE CHOICE WAS MADE BY A SCOREBOARD rather than by a separate point, and that is a matter of principle: at one
/// point the answer came out the opposite depending on WHOSE client it was.
///   * alternating boots, a Linux client, the knob only for the server: the pair is better
///     (p99 80.5 against 133.7) — but there only the server is measured;
///   * the SUITE's configuration (both sides Talaria, the client is ours), the full scoreboard,
///     55 points by three repeats: the honest sign is BETTER by a crushing score.
///     mysql: the median +9 =1 -0 and p99 +10 =0 -0 against +0 =0 -10 and +3 =0 -7 for
///     the pair; postgresql's p99 +5 =2 -3 against +2 =0 -8.
/// The reason is visible in the setup itself: in the suite our client polls the stack too,
/// and its latency enters the measured number — the saving on the sign is won back
/// on both sides at once.
///
/// The comparison of the pair stays behind a knob: it is cheaper and correct in essence (a comparison
/// of the VALUES alone went by different addresses, because a pass moves the head),
/// and on a workload where the client is a foreign one it wins.
pub static RING_HONEST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
pub fn ringhonest_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.ringhonest") {
        let on = v != "0";
        RING_HONEST.store(on, core::sync::atomic::Ordering::SeqCst);
        crate::println!("netsys: the sign \"the card is silent\" — {}", if on { "the honest one (the ring's phase bit)" } else { "THE FORMER one (a comparison \
            of the word's value)" });
    }
}
/// Can the driver answer "is there a received frame" (see `poll_gated`).
static RX_READY_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static RING_ADDR_NOW: AtomicU64 = AtomicU64::new(0);
static RING_ADDR_LAST: AtomicU64 = AtomicU64::new(0);

/// Create the window for watching the receive ring. Only when the interface is ONE:
/// otherwise the silence of the first card says nothing about the second.
fn ring_watch_setup() {
    if POLL_HARD_NS.load(core::sync::atomic::Ordering::SeqCst) == 0 { return; } // talaria.pollhard=0
    if unsafe { NIF } != 1 { return; }
    let Some(f) = ifopt(0) else { return };
    let Some((_, v)) = f.dev.idle_watch() else { return };
    RING_LAST.store(v, core::sync::atomic::Ordering::SeqCst);
    RING_WATCH.store(true, core::sync::atomic::Ordering::SeqCst);
    crate::println!("netsys: an idle pass of the stack is cancelled BY THE receive RING (a pass for the sake of the timers happens not less often than once \
        every {} us)",
        POLL_HARD_NS.load(core::sync::atomic::Ordering::Relaxed) / 1000);
}

pub fn pollgap_from_cmdline(cmdline: &str) {
    if let Some(ns) = crate::cmdline::num(cmdline, "talaria.pollgap") {
        POLLGAP_NS.store(ns, core::sync::atomic::Ordering::SeqCst);
        crate::println!("netsys: an idle pass of the stack not more often than once every {} ns", ns);
    }
    if crate::cmdline::num(cmdline, "talaria.nettrace").unwrap_or(0) != 0 {
        NET_TRACE.store(true, core::sync::atomic::Ordering::SeqCst);
        crate::println!("netsys: the connections' trace is on (net: connect/close)");
    }
    if let Some(ns) = crate::cmdline::num(cmdline, "talaria.pollhard") {
        // 0 — go back to the former threshold by time (the window over the ring is not created).
        POLL_HARD_NS.store(ns, core::sync::atomic::Ordering::SeqCst);
        crate::println!("netsys: a pass of the stack for the sake of the timers not less often than once every {} ns{}", ns,
            if ns == 0 { " (the watching of the ring is off)" } else { "" });
    }
}

// ── A CORE WITHOUT WORK DRIVES THE STACK (`talaria.idlepoll=<ns>`) ───────────
//
// WHAT FOR. There are no interrupts from the card in this build: the stack is driven only by the thread
// that already sits in a network syscall. Which means a frame that arrived at the moment when
// a NON-network neighbour computes on the core is seen by nobody until a tick — while a tick on a core with
// a network waiter is `netslice` = 100 us. It is exactly that mode that is written down in
// `netirq_setup`: "a frame that arrived at an empty machine waited for a TICK".
//
// Meanwhile A CORE WITHOUT TASKS at that moment SLEEPS. `sched::ap_loop`, having found no
// work, goes into a `kwait_once(DOORBELL)`, that is, into a `hlt` until a call; the call is
// rung at a task's migration, while for an arrived frame — never. On the postgresql workload
// at 4093 req/s and a latency of 60 us 0.25 of a core out of four is busy:
// three and a half cores slept while a frame lay in the ring.
//
// WHAT IS DONE. A core that has nothing to execute, instead of sleeping, drives a pass of
// the stack — but only while THE NETWORK IS ALIVE (frames arrived recently). That way an idling
// machine still sleeps, while a loaded one parses the ring immediately.
//
// THE PRICE. An idle pass with a silent card does not take the stack's lock at all: the question
// is asked of the ring (`rx_ready` — two reads, without the lock), and only a positive
// answer leads to a pass. That is, a spin on an empty core does not compete with the working
// threads for the machine's lock.
//
// WHY THIS IS NOT A REPLACEMENT FOR THE CARD'S LINE. A core without tasks does not always exist; when all
// the cores are busy, a frame still waits for the one that enters a syscall. The mechanism
// removes ONE case out of two — but it requires neither an interrupt nor a parking.
/// The window between the rechecks of "has work appeared" (ns). Not "how long
/// to spin before giving up": while the network is alive, the loop enters here again and again.
/// 0 — switch it off (sleep, as it was).
///
/// THE DEFAULT IS ZERO, AND THAT IS A MEASUREMENT rather than caution. The mechanism did not fire A
/// SINGLE TIME in any checked configuration (the stand of 11 September, mysql, three
/// tails in a row): `IDLE_POLLS` stayed a zero both with the polling and with the parking.
///
/// THE REASON HAS BEEN FOUND, AND IT IS IN THE PLACE OF THE INSERTION. A waiter that went to sleep goes into a `hlt`
/// INSIDE ITS OWN CONTEXT (`sync::kwait_once`) without returning the core into
/// `sched::ap_loop` — while the hook stands exactly there. Which means "a core without work" in the
/// sense in which `ap_loop` sees it does not arise on a network workload: the core
/// is always busy either with a spinning or with a sleeping waiter.
///
/// For the mechanism to work, the insertion is needed IN THE PLACE OF THE SLEEP rather than in the pickup loop —
/// and that is separate work, which now has a measured basis. The knob is
/// kept so that it can be switched on with one line when the insertion moves.
pub static IDLEPOLL_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The network is considered ALIVE for this long after the last received frame.
const NET_LIVE_NS: u64 = 20_000_000; // 20 ms
/// The cycle of the last pass that really brought frames.
static LAST_RX_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many times an empty core drove the stack instead of sleeping and how many times that brought
/// a frame. THE SECOND NUMBER IS THE ONLY THING THE MECHANISM WAS INTRODUCED FOR: those are the frames
/// that would otherwise have waited for a tick. A zero in it means that the mechanism is not needed.
pub static IDLE_POLLS: AtomicU64 = AtomicU64::new(0);
pub static IDLE_POLL_RX: AtomicU64 = AtomicU64::new(0);
/// A TOKEN: the ring is ONE, the socket set is ONE, the stack's lock is ONE — which means it makes sense
/// for ONE empty core to drive them. Without a token four free cores
/// would poll one and the same cache line and fight for one and the same lock,
/// that is, the mechanism would fix the latency while spoiling the throughput. The rest
/// sleep as before: the work that appears for them will wake them with a call.
static IDLE_POLLER: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

pub fn idlepoll_from_cmdline(cmdline: &str) {
    if let Some(ns) = crate::cmdline::num(cmdline, "talaria.idlepoll") {
        IDLEPOLL_NS.store(ns, core::sync::atomic::Ordering::SeqCst);
        crate::println!("netsys: a core without work drives the stack in windows of {} ns{}", ns,
            if ns == 0 { " (it is off — it sleeps, as it was)" } else { "" });
    }
}

/// Did the one that has nothing to execute drive the frames. `bail` means "work has appeared".
/// It returns true if the loop took place (the caller need not sleep — let it
/// recheck the tasks).
pub fn idle_poll(mut bail: impl FnMut() -> bool) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let win = IDLEPOLL_NS.load(Relaxed);
    if win == 0 { return false; }
    if unsafe { NIF } == 0 { return false; }
    let hz = unsafe { HZ };
    if hz == 0 { return false; }
    let now = tsc::rdtsc();
    // IS THE NETWORK ALIVE? The sign is that frames arrived recently. Otherwise an empty machine
    // would spin forever, while nobody prevents it from sleeping: while the server listens,
    // its accepting thread is READY anyway, that is, the core is not empty.
    let last = LAST_RX_TSC.load(Relaxed);
    if last == 0 { return false; }
    let live = (hz / 1_000_000).max(1) * (NET_LIVE_NS / 1000);
    if now.wrapping_sub(last) > live { return false; }
    // The token is taken for ONE window and given back at the exit — including by a `bail`.
    let me = crate::percpu::cpu_id();
    if IDLE_POLLER.compare_exchange(usize::MAX, me, core::sync::atomic::Ordering::AcqRel,
                                    core::sync::atomic::Ordering::Relaxed).is_err() { return false; }
    IDLE_POLLS.fetch_add(1, Relaxed);
    let rx0 = crate::l1::rx_seen();
    let win_cy = (hz / 1_000_000).max(1) * (win / 1000).max(1);
    loop {
        poll();
        if bail() { break; }
        if tsc::rdtsc().wrapping_sub(now) >= win_cy { break; }
        core::hint::spin_loop();
    }
    if crate::l1::rx_seen() != rx0 { IDLE_POLL_RX.fetch_add(1, Relaxed); }
    IDLE_POLLER.store(usize::MAX, core::sync::atomic::Ordering::Release);
    true
}

pub fn poll() { poll_gated(false) }
/// A pass of the stack WITHOUT the `pollgap` throttling. It is needed by the card line's handler: it knows
/// that there is a frame (the interrupt has come), while the throttling looks at the receive counter,
/// which has not moved before the pass. A throttled pass in the handler is
/// a frame left in the ring until somebody's syscall, that is, exactly that
/// delivery "when they get round to it" which the interrupt removes.
pub fn poll_forced() { poll_gated(true) }
fn poll_gated(force: bool) {
    let mut stale = false; // the pass is allowed by the debt to the timers rather than by the ring
    // The threshold is taken BEFORE the lock: the point is not to take the lock at all.
    {   use core::sync::atomic::Ordering::Relaxed;
        let gap = POLLGAP_NS.load(Relaxed);
        if gap != 0 && !force {
            // THE FIRST QUESTION GOES TO THE RING (see `ring_word`): the card has brought
            // nothing new -> there is nothing to parse, and there is no point in waiting for the gap.
            // What remains is only the debt to the stack's own timers.
            // THE FIRST QUESTION GOES TO THE RING, AND WE COMPARE NOT ONLY THE VALUE BUT ALSO THE ADDRESS.
            //
            // The sign "the card has brought nothing" was built by comparing the WORD of the
            // receive ring between the passes. A pass MOVES the ring's head,
            // so values at DIFFERENT addresses were compared: the word of the previous
            // head against the word of the new one. They coincided by chance — the sign said
            // "it is silent" with an arrived frame, and the frame waited for the timers' debt
            // (100 us). On the stand that is 3841 lies over 280 thousand requests = 1.4 %,
            // exactly the share of the requests in the p99 plateau (the stage "the socket->the reader" p50 7 us
            // with p99 56.5 and p99.9 113 — those very 100 us of debt plus the processing).
            //
            // The head has moved — there is nothing to compare with, and the pass is made: that is
            // not a loss, the head moves only when WE OURSELVES took the frames.
            // THE RING'S HONEST ANSWER IF THE DRIVER CAN GIVE IT. A comparison of the word's values
            // (below) cannot be reliable in principle: the device is entitled to
            // write a descriptor whose watched dword coincided with the previous one, and then
            // the sign stays silent with an arrived frame. ENA has a phase bit — the very
            // one by which the driver itself takes a frame.
            // A BUSY DEVICE IS NO REASON TO GO BACK TO A LYING SIGN.
            // `rx_ready` answers None in two different cases: the driver cannot do it
            // (virtio) and the driver is BUSY RIGHT NOW with a foreign pass. In the second case
            // falling back to a comparison of the values is not allowed — it is exactly what lies; the right
            // answer is "make a pass": it will either not be given (the lock is at a neighbour, and the frame
            // will be parsed by it) or it will parse the frame itself. Whether the driver can answer —
            // we remember from the first successful answer.
            let rr = if RING_HONEST.load(Relaxed) { ifopt(0).and_then(|f| f.dev.rx_ready()) } else { None };
            if rr.is_some() { RX_READY_OK.store(true, Relaxed); }
            let rr = match rr { Some(v) => Some(v), None if RX_READY_OK.load(Relaxed) => Some(true), None => None };
            if let Some(r) = rr {
                if r { /* there is a frame (or the device is busy) — a pass immediately */ }
                else {
                    let now = tsc::rdtsc();
                    RING_EMPTY_TSC.store(now, Relaxed);
                    if let Some(f) = ifopt(0) { f.dev.tx_watch(); } // session 32: the latency of the transmission's completion
                    let hard = POLL_HARD_NS.load(Relaxed);
                    let need = (unsafe { HZ } / 1_000_000).max(1) * (hard / 1000).max(1);
                    if now.wrapping_sub(LAST_POLL_TSC.load(Relaxed)) < need {
                        POLL_SKIPPED.fetch_add(1, Relaxed);
                        return;
                    }
                    stale = true;
                }
            } else {
            match ring_watch_now() {
                (None, Some(w)) => {
                    if w == RING_LAST.load(Relaxed)
                        && RING_ADDR_NOW.load(Relaxed) == RING_ADDR_LAST.load(Relaxed) {
                        let now = tsc::rdtsc();
                        let hard = POLL_HARD_NS.load(Relaxed);
                        let need = (unsafe { HZ } / 1_000_000).max(1) * (hard / 1000).max(1);
                        if now.wrapping_sub(LAST_POLL_TSC.load(Relaxed)) < need {
                            POLL_SKIPPED.fetch_add(1, Relaxed);
                            return;
                        }
                        // A pass BY THE DEBT: the card, by its own word, was silent. If it
                        // brings frames, the word lied (see RING_LIED).
                        stale = true;
                    }
                }
                // There is no window (there are several cards or the driver does not give the word) —
                // the former threshold by time.
                _ => {
                    let rx = crate::l1::rx_seen();
                    if rx == LAST_POLL_RX.load(Relaxed) {
                        let now = tsc::rdtsc();
                        let need = (unsafe { HZ } / 1_000_000).max(1) * gap / 1000;
                        if now.wrapping_sub(LAST_POLL_TSC.load(Relaxed)) < need {
                            POLL_SKIPPED.fetch_add(1, Relaxed);
                            return;
                        }
                    }
                }
            }
            }
        }
    }
    let _g = match net_try() { Some(g) => g, None => return }; // the stack is already being driven — a second pass is not needed
    nw_ph(PH_PASS);
    // The debt of a missed interrupt is extinguished by ANY pass that took place: it
    // will parse the same ring (see NIC_PENDING).
    // A swap, not a store: a plain store does not order this pass's ring loads after it,
    // and a handler's `true` landing just before could be erased by a `false` that the
    // ring read did not cover. The RMW reads the handler's flag (Acquire) and the reads of
    // the ring follow it. Read first, so the common path writes no shared line.
    if NIC_PENDING.load(core::sync::atomic::Ordering::Relaxed) { NIC_PENDING.swap(false, core::sync::atomic::Ordering::AcqRel); }
    // We count the passes by the debt THAT TOOK PLACE: counting them before the lock is not allowed, the lock
    // may not be given.
    if stale { POLL_HARD_RUNS.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
    let rx0 = crate::l1::rx_seen();
    let t_pass = tsc::rdtsc();
    // The interval (a): from what moment the ring could have been non-empty unnoticed.
    let since = LAST_POLL_TSC.load(core::sync::atomic::Ordering::Relaxed)
        .max(RING_EMPTY_TSC.load(core::sync::atomic::Ordering::Relaxed));
    {   use core::sync::atomic::Ordering::Relaxed;
        LAST_POLL_TSC.store(t_pass, Relaxed);
        LAST_POLL_RX.store(rx0, Relaxed);
        // The ring's value is taken BEFORE the pass: everything that arrives during it
        // must count as NEW (otherwise a frame that arrived in the middle of a pass
        // would stay unparsed until the timers' debt).
        if let Some(w) = ring_word() {
            RING_LAST.store(w, Relaxed);
            RING_ADDR_LAST.store(RING_ADDR_NOW.load(Relaxed), Relaxed);
        }
    }
    reap_orphans(); // before the pass: the untied ones go away, the overdue ones get an RST in this same pass
    rearm_listeners(); // the freed slots go back into the listeners' backlog
    let n = unsafe { NIF };
    for idx in 0..n {
        let f = match ifopt(idx) { Some(f) => f, None => continue };
        if !f.info.up { continue; }
        let ts = nowi(unsafe { HZ });
        // The profile's marking (§7.1) is kept by THE FIRST interface: the datapath's numbers
        // are compared between sessions, and mixing the management card into them is not allowed.
        let t0 = if idx == 0 && crate::netprof::on() { tsc::rdtsc() } else { 0 };
        f.iface.poll(ts, &mut f.dev, &mut f.sockets);
        // §7.4: the kernel echo — the answer is formed right here, before the exit from the pass of the stack
        if let Some(h) = f.ke {
            let mut buf = [0u8; 2048];
            let mut echoed = false;
            loop {
                let (len, ep) = match f.sockets.get_mut::<udp::Socket>(h).recv() {
                    Ok((d, m)) => { let l = d.len().min(buf.len()); buf[..l].copy_from_slice(&d[..l]); (l, m.endpoint) }
                    Err(_) => break,
                };
                // §4.7 (A1): the marking of the INTERNAL part of the kernel echo's turnaround.
                if !echoed { crate::netprof::mark_rx_at(t0); crate::netprof::mark_app_in(); echoed = true; }
                if unsafe { CFG_KTS } && len >= 8 {
                    buf[..8].copy_from_slice(&crate::tsc::wall_ns().to_le_bytes()); // §4.9 one-way
                }
                let _ = f.sockets.get_mut::<udp::Socket>(h).send_slice(&buf[..len], ep);
            }
            if echoed { crate::netprof::mark_app_out(); }
            f.iface.poll(ts, &mut f.dev, &mut f.sockets); // push the answer out in the same pass
            if echoed { f.dev.flush(); crate::netprof::mark_tx(); }
        }
        // A3: the sink — scoop out and throw away (otherwise the buffer will overflow and the stack will start
        // dropping packets EARLIER than the receive counter).
        if let Some(h) = f.ks {
            while f.sockets.get_mut::<udp::Socket>(h).recv().is_ok() {}
        }
        f.dev.flush(); // §7.2: one call to the device for the whole pass of the stack
        if t0 != 0 { crate::netprof::mark_poll(tsc::rdtsc().wrapping_sub(t0)); } // §7.1
        if f.dh.is_some() { dhcp_process(idx); }
        if f.nt.is_some() { ntp_process(idx, ts); }
    }
    if crate::l1::rx_seen() != rx0 {
        if stale { RING_LIED.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
        dla_note(t_pass, since);
        rxgap_note(t_pass, stale, crate::l1::rx_seen().wrapping_sub(rx0));
        // "The network is alive" for the idle pass of an empty core (see `idle_poll`).
        LAST_RX_TSC.store(tsc::rdtsc(), core::sync::atomic::Ordering::Relaxed);
        deliver_ready();
    }
}

/// TO WHOM THE DATA CAME — AND WHERE IT IS NOW.
///
/// A frame parsed by the stack gets into the socket's buffer, but it can be taken only by
/// THE OWNER of that socket (the thread that blocked on it in a `read`/
/// `epoll_wait`). The stack could have been driven by anybody — any thread in any network
/// syscall. So "the data have come" and "they have been taken" are separated by a wait, and
/// session 21's measurement showed what that costs: mongod's stage `the socket->the reader` has
/// p50 = 1.4 us with p99 = 182 us, that is, in a hundredth of the cases a frame lies in
/// the socket for almost two quanta.
///
/// Here this moment is caught exactly once — at the TRANSITION of the socket into "there are
/// data" — and the processor is given to the owner: on its own core by a hint
/// to the scheduler, on a foreign one by a hint and a call (the same trick as in
/// `sync::wake_addr`, where it has already been paid for by a measurement).
///
/// THE PRICE. The walk over the 32 slots is made ONLY on the passes of the stack that
/// really received a frame: with mongod the stack is driven 350-420 times per turnaround, and
/// scanning at every one would be more expensive than the delivery itself.
fn deliver_ready() {
    let prof = crate::netprof::on();
    let now = if prof { tsc::rdtsc() } else { 0 };
    let me = crate::sched::current_id() as u8;
    let mycpu = crate::percpu::cpu_id();
    for i in 0..NS {
        if tt()[i].k != K::Conn || tt()[i].h.is_none() { continue; }
        // READY TO BE READ IS NOT ONLY DATA. The peer closed the connection — the socket
        // is readable too: a `read` will return 0, a `poll`/`epoll` must return an EPOLLIN
        // (and an EPOLLHUP), and a waiter must wake up. Here for years there stood
        // one `can_recv()`, that is, the call went ONLY on data, while the end of
        // a connection was learnt by a waiter from the ceiling: a measurement on the gate `netsolo` gave
        // exactly one miss per connection (4 out of 124 waits), and in all
        // the cases the waiter slept THE FULL 5 ms of the backstop while having the readiness
        // 0x2011 = IN|HUP in its hands. With a workload with short connections that is
        // a ceiling at EVERY closing.
        let readable = s(i).can_recv() || !s(i).may_recv() || tt()[i].err != 0;
        if !readable { tt()[i].rdy = 0; tt()[i].armed = false; tt()[i].dl = 0; continue; }
        if prof && tt()[i].rdy == 0 { tt()[i].rdy = now; }
        // A TRANSITION rather than a state: while the owner has not taken the data, the socket
        // stays ready, and calling it at every pass would mean sending
        // a call thousands of times per frame.
        if tt()[i].armed { continue; }
        tt()[i].armed = true;
        let t = tt()[i].tid;
        // Session 32's instrument: in WHAT state the reader is at the moment when its data
        // landed in the socket. It is closed in `read` (`dl_close`).
        tt()[i].dl = tsc::rdtsc();
        NET_LAST_DL.store(tt()[i].dl, core::sync::atomic::Ordering::Relaxed);
        tt()[i].trdla = LAST_DLA_US.load(core::sync::atomic::Ordering::Relaxed);
        tt()[i].dlc = dl_class(t, me);
        tt()[i].dlx = if t != 255 && (t as usize) < crate::sched::NTASKS { crate::sched::core_ctx_of(t as usize) } else { 0 };
        tt()[i].dlo = dl_occupant(t, tt()[i].dlx);
        {
            let o = (tt()[i].dlx & 0xFFFF) as usize;
            if o < crate::sched::NTASKS && o != t as usize {
                let (mut pt, mut why) = ([0u64; 16], [0u32; WHY_N]);
                nw_snap(o, &mut pt, &mut why);
                tt()[i].dlpt = pt; tt()[i].dlwhy = why;
            }
        }
        if t != 255 && (t as usize) < crate::sched::NTASKS {
            tt()[i].dlr = crate::sched::run_ns(t as usize);
            tt()[i].dls = crate::sched::switches(t as usize);
            tt()[i].dlp = RD_PASS[t as usize].load(core::sync::atomic::Ordering::Relaxed);
            tt()[i].dlcpu = crate::sched::cpu_of(t as usize) as u8;
            tt()[i].dlby = mycpu as u8;
        }
        // WAKE THE OWNER. It could have been not simply waiting for the processor but SLEEPING
        // (`wait_frame`): then the call is not the scheduler's preference but
        // the only thing that will bring it back to life earlier than the parking's ceiling.
        wake_owner(t);
        if t == 255 { DELIV_NOOWNER.fetch_add(1, core::sync::atomic::Ordering::Relaxed); continue; }
        if t == me { DELIV_SELF.fetch_add(1, core::sync::atomic::Ordering::Relaxed); continue; }
        // HERE WE TRIED GIVING THE PROCESSOR TO THE OWNER — AND THAT IS WORSE.
        //
        // It seemed obvious: since a frame waits for a reader (the stage `the socket->the reader`
        // p99 = 182 us), the reader must be called at once — by a hint to the scheduler
        // on its own core, by a hint and a call on a foreign one. The measurement said the opposite:
        // the request's p99 246 -> 281 and 305 us, while THE MEDIAN of the delivery got worse
        // fivefold (1.4 us -> 6.8-9.1). The reason is visible in the counters below:
        // the owner sits on a foreign core in about half of the cases, and a call
        // there is a context switch with a reload of CR3 at EVERY
        // appearance of data. It costs more than it saves, and it destroys the fast
        // case, for the sake of a tail that did not move by a single bucket
        // (the stage's p99 stayed exactly 181936 ns).
        //
        // Only THE MARKING is left: the counters answer the question "is there
        // anybody to hand over to at all" — and they answer that there is, but one must not hand over.
        let c = crate::sched::cpu_of(t as usize);
        if c == mycpu { DELIV_SAMECPU.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
        else { DELIV_OTHERCPU.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
    }
    // A NEW CONNECTION IS A DELIVERY TOO. A waiter in an `accept4` (and a waiter in an
    // `epoll_wait` on the listener) sleeps on ITS OWN word, while a connection comes into
    // a backlog slot, which has no owner: the owner belongs to THE LISTENER.
    // Without this walk the accepting thread would sleep until the ceiling at every
    // connection — that is, the parking would fix a request and break a connection.
    reap_backlog();
    for i in 0..NS {
        if tt()[i].k != K::Backlog || tt()[i].h.is_none() { continue; }
        if !s(i).is_active() { tt()[i].armed = false; continue; }
        if tt()[i].armed { continue; }
        tt()[i].armed = true;
        let l = tt()[i].owner as usize;
        if l < NS && tt()[l].k == K::Listen { wake_owner(tt()[l].tid); }
    }
    // UDP: the same transition "data have appeared in the buffer" -> a call to the owner.
    for i in 0..NU {
        if !uu()[i].used || uu()[i].h.is_none() { continue; }
        if !us(i).can_recv() { uu()[i].armed = false; continue; }
        if uu()[i].armed { continue; }
        uu()[i].armed = true;
        wake_owner(uu()[i].tid);
    }
}

// ── THE DELIVERY -> THE READER BY THE READER'S STATE (session 32, step 1.1) ──
//
// WHAT FOR. The common histogram "ready->cpu" (`sched::RQ_HIST`) gives ~29 intervals per
// a mysql request, almost all of them idle yields of the waiters; the tail of one
// request cannot be found in it. The stage `the socket->the reader` (`netprof::ST_DELIV`) is honest,
// but it does not say WHY the reader did not take the data: it slept in a hlt (H1), stood in
// a queue behind a foreign quantum (H2), spun in ring 3 about its own business (H4).
// So at the moment of the delivery (`deliver_ready`, the socket's transition into "there are data")
// the owner's STATE is remembered, while in `read` — how much passed until the pickup,
// with a separate histogram per state:
//   0 "itself"      — the stack was driven by the owner itself: the pure price of a pass;
//   1 "it runs, it waits for the network" — on another core, inside a network syscall (a spinner,
//                    there is no parking): it must take it within microseconds — the stack's lock
//                    or the scan's rejection, if not;
//   2 "it runs, not in the network" — on another core in ring 3 / another syscall: the time
//                    goes at THE APPLICATION (H4), the kernel has nothing to do with it;
//   3 "ready, not running" — preempted: it waits for the quantum of the one on its core (H2);
//                    for the worst case who was there is printed;
//   4 "asleep on the network" — parked on the `NW_WORD`: a call + the exit from the hlt (H1);
//   5 "asleep on another word" — futex/nanosleep: the application went to sleep itself;
//   6 "without an owner" — nobody was waiting for the socket (the first read after an accept and so on).
// Plus the interval (a) — "the ring -> the pass": an upper estimate of how long the frame lay in
// the ring unnoticed (from the last honest answer "the ring is empty" / the end of
// the previous pass to the start of the pass that took it).
pub const DLC_N: usize = 7;
pub const DLC_NAMES: [&str; DLC_N] = ["itself", "it runs, it waits for the network", "it runs, not in the network", "ready, not running",
                                      "asleep on the network", "asleep on another word", "without an owner"];
/// The histogram by class, us: <5, <10, <20, <50, <100, <200, <500, 500+.
pub static DL_HIST: [[AtomicU64; 8]; DLC_N] = [const { [const { AtomicU64::new(0) }; 8] }; DLC_N];
pub static DL_SUM_US: [AtomicU64; DLC_N] = [const { AtomicU64::new(0) }; DLC_N];
pub static DL_MAX_US: [AtomicU64; DLC_N] = [const { AtomicU64::new(0) }; DLC_N];
/// The circumstances of the worst one by class: the low 16 bits are the task on the owner's core
/// at the delivery (0xFFFF — a hlt), the higher ones are that core's switches until the pickup.
pub static DL_MAX_CTX: [AtomicU64; DLC_N] = [const { AtomicU64::new(0) }; DLC_N];
/// How many deliveries of the class found the owner's core EMPTY (a hlt) — the share for H1.
pub static DL_IDLE: [AtomicU64; DLC_N] = [const { AtomicU64::new(0) }; DLC_N];
/// The interval (a) "the ring -> the pass", the same grid of buckets; it is counted per PASS
/// that brought frames.
pub static DLA_HIST: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
pub static DLA_MAX_US: AtomicU64 = AtomicU64::new(0);
/// The last honest answer "the ring is empty" (`rx_ready() == Some(false)`).
static RING_EMPTY_TSC: AtomicU64 = AtomicU64::new(0);
/// Is the task inside a network wait syscall right now (see `NetWaiter`).
/// Is the thread `t` spinning right now in a network wait (`poll`/`epoll_wait`/`recv`).
/// THE GATE (`reqfirst`): declare a thread a network waiter without the wait itself — otherwise
/// the scheduler policy's decision is checked only on a live network.
pub fn nw_probe(t: usize, on: bool) {
    if t < crate::sched::NTASKS { NW_IN[t].store(on, core::sync::atomic::Ordering::Relaxed); }
}
pub fn is_net_waiter(t: usize) -> bool {
    t < crate::sched::NTASKS && NW_IN[t].load(core::sync::atomic::Ordering::Relaxed)
}
static NW_IN: [core::sync::atomic::AtomicBool; crate::sched::NTASKS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; crate::sched::NTASKS];

/// The SLOW deliveries (>= 50 us — what the p99 is made of): who occupied the reader's
/// core at the moment of the delivery (0 a hlt, 1 a network waiter, 2 other), how many
/// switches that core made during the wait (0/1/2/3+), did the scheduler see
/// the reader at all (`runnable_peek`), and which task occupied the core — by number.
pub const DL_SLOW_US: u64 = 50;
pub static DLS_OCC: [[AtomicU64; 3]; DLC_N] = [const { [const { AtomicU64::new(0) }; 3] }; DLC_N];
pub static DLS_SW: [[AtomicU64; 4]; DLC_N] = [const { [const { AtomicU64::new(0) }; 4] }; DLC_N];
pub static DLS_HIDDEN: [AtomicU64; DLC_N] = [const { AtomicU64::new(0) }; DLC_N];
pub static DLS_TASK: [AtomicU64; crate::sched::NTASKS] = [const { AtomicU64::new(0) }; crate::sched::NTASKS];
/// The slow deliveries — WHAT THE READER WAS BUSY WITH during the wait: the share of its
/// processor time in the interval (<10 %, <50 %, <90 %, >=90 %), how many times
/// it was itself taken off the core (0/1/2/3+) and how many passes of the stack from `read`
/// it made without seeing the data (0 / 1-2 / 3-10 / 11+).
pub static DLS_RAN: [[AtomicU64; 4]; DLC_N] = [const { [const { AtomicU64::new(0) }; 4] }; DLC_N];
pub static DLS_OWNSW: [[AtomicU64; 4]; DLC_N] = [const { [const { AtomicU64::new(0) }; 4] }; DLC_N];
pub static DLS_PASS: [[AtomicU64; 4]; DLC_N] = [const { [const { AtomicU64::new(0) }; 4] }; DLC_N];
/// The passes of the stack made by a task from the `read` loop (see `RD_PASS` in read).
pub static RD_PASS: [AtomicU64; crate::sched::NTASKS] = [const { AtomicU64::new(0) }; crate::sched::NTASKS];
/// Who occupied the reader `t`'s core at the delivery (`x` is `core_ctx_of(t)`), plus bit 2:
/// the reader at that moment is NOT visible to the scheduler (parked in a futex wait).
fn dl_occupant(t: u8, x: u64) -> u8 {
    use core::sync::atomic::Ordering::Relaxed;
    if t == 255 || t as usize >= crate::sched::NTASKS { return 0; }
    let o = (x & 0xFFFF) as usize;
    let kind = if o == 0xFFFF { 0 } else if o < crate::sched::NTASKS && NW_IN[o].load(Relaxed) { 1 } else { 2 };
    let hidden = if crate::sync::runnable_peek(t as usize) { 0 } else { 4 };
    kind | hidden
}
#[inline] fn dl_bucket(us: u64) -> usize {
    match us { 0..=4 => 0, 5..=9 => 1, 10..=19 => 2, 20..=49 => 3, 50..=99 => 4, 100..=199 => 5, 200..=499 => 6, _ => 7 }
}
/// The class of the owner `t`'s state at the moment of the delivery (`me` is the one that drove the stack).
fn dl_class(t: u8, me: u8) -> u8 {
    use core::sync::atomic::Ordering::Relaxed;
    if t == 255 || t as usize >= crate::sched::NTASKS { return 6; }
    if t == me { return 0; }
    let t = t as usize;
    let w = crate::sync::waiting_on(t);
    if w != 0 { return if w == nw_addr(t) { 4 } else { 5 }; }
    if crate::sched::running_on(t).is_some() { return if NW_IN[t].load(Relaxed) { 1 } else { 2 }; }
    3
}
/// THE NETWORK WAITER'S PHASES — a ring per task: WHERE inside the wait loop the thread
/// was and how many times in a row it went through the same phase. It was introduced for session
/// 32's analysis: the ring of the scheduler's decisions showed that a waiter on the reader's
/// core DOES NOT YIELD for ~100 us (with a yield once every 5 us before and after), while by
/// the switches alone one cannot say whether it spun in the loop with the yield window closed,
/// stood in one call (the lock, a pass) or was in ring 3 altogether.
/// A record happens only at a change of phase (a repeat of the same phase increases the counter),
/// the price is a comparison and a write into one's own line.
#[derive(Clone, Copy)]
pub struct PhEv { pub tsc: u64, pub n: u32, pub ph: u8 }
pub const PHE0: PhEv = PhEv { tsc: 0, n: 0, ph: 255 };
pub const PH_N: usize = 48;
pub static mut NW_PH: [[PhEv; PH_N]; crate::sched::NTASKS] = [[PHE0; PH_N]; crate::sched::NTASKS];
pub static NW_PH_POS: [core::sync::atomic::AtomicU32; crate::sched::NTASKS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; crate::sched::NTASKS];
pub const PH_ENTER: u8 = 1;  // the entry into a poll(2)/epoll_wait/read
pub const PH_TOP: u8 = 2;    // the top of the wait loop
pub const PH_POLL: u8 = 3;   // we call a pass of the stack (it could have been rejected)
pub const PH_PASS: u8 = 4;   // a pass of the stack TOOK PLACE (the lock is ours)
pub const PH_LOCK: u8 = 5;   // we wait for the lock for the scan of the set
pub const PH_SCAN: u8 = 6;   // the scan of the set under the lock
pub const PH_WIN: u8 = 7;    // we entered the yield window
pub const PH_SPIN: u8 = 8;   // the window: there is nobody to yield to/it is too early — a spin
pub const PH_YIELD: u8 = 9;  // we yielded the core (by address or by a rotation)
pub const PH_BACK: u8 = 10;  // we came back from the yield
pub const PH_NIC: u8 = 11;   // a pass by the interrupt's debt (NIC_PENDING)
pub const PH_PARK: u8 = 12;  // we went to sleep on the word
pub const PH_RET: u8 = 13;   // the exit from the wait with a ready set
pub const PH_READ: u8 = 14;  // the entry into read
pub const PH_WRITE: u8 = 15; // the entry into write
pub const PH_NAMES: [&str; 16] = ["?", "the entry", "the top", "the poll", "the pass", "the lock", "the scan", "the window", "the spin",
    "the yield", "we came back", "NIC", "the parking", "the exit", "read", "write"];
/// THE TIME BY PHASE PER TASK (TSC cycles): a ring of 48 records covers ~3 us
/// of the wait loop, while a reader waits 100-200 us — by the ring one cannot say where
/// the one occupying the core spent THOSE hundred microseconds. An accumulator per phase gives that
/// as the difference of two snapshots (at the delivery and at the read).
pub static NW_PH_T: [[core::sync::atomic::AtomicU64; 16]; crate::sched::NTASKS] =
    [const { [const { core::sync::atomic::AtomicU64::new(0) }; 16] }; crate::sched::NTASKS];
/// THE OUTCOMES OF THE YIELD WINDOW PER TASK (see `yield_window`): why a waiter did NOT give
/// the core up — or did. The indices are `WHY_*`.
pub const WHY_N: usize = 8;
pub const WHY_NAMES: [&str; WHY_N] = ["non-threaded", "netyield=0", "nobody and without a pull", "too early",
    "the stack's lock is busy", "there are no ready ones", "a foreign one, the pull did not work out", "it yielded"];
pub static NW_WHY: [[core::sync::atomic::AtomicU32; WHY_N]; crate::sched::NTASKS] =
    [const { [const { core::sync::atomic::AtomicU32::new(0) }; WHY_N] }; crate::sched::NTASKS];
#[inline] fn nw_why(t: usize, k: usize) {
    if t < crate::sched::NTASKS { NW_WHY[t][k].fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
}
/// `ready_peer` did not enter under the stack's lock (per task, for `nw_why`).
static RP_BUSY: [core::sync::atomic::AtomicU32; crate::sched::NTASKS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; crate::sched::NTASKS];
#[inline]
pub fn nw_ph(ph: u8) {
    use core::sync::atomic::Ordering::Relaxed;
    let t = crate::sched::current_id();
    if t >= crate::sched::NTASKS { return; }
    let p = NW_PH_POS[t].load(Relaxed) as usize;
    let ring = unsafe { &mut *core::ptr::addr_of_mut!(NW_PH[t]) };
    let now = tsc::rdtsc();
    if p != 0 {
        let last = &mut ring[(p - 1) % PH_N];
        if last.ph == ph { last.n = last.n.saturating_add(1); return; }
        NW_PH_T[t][(last.ph as usize).min(15)].fetch_add(now.wrapping_sub(last.tsc), Relaxed);
    }
    ring[p % PH_N] = PhEv { tsc: now, n: 1, ph };
    NW_PH_POS[t].store((p + 1) as u32, Relaxed);
}
/// A snapshot of the task `t`'s accumulators (for the difference in `dl_dump`).
fn nw_snap(t: usize, pt: &mut [u64; 16], why: &mut [u32; WHY_N]) {
    use core::sync::atomic::Ordering::Relaxed;
    if t >= crate::sched::NTASKS { return; }
    for k in 0..16 { pt[k] = NW_PH_T[t][k].load(Relaxed); }
    for k in 0..WHY_N { why[k] = NW_WHY[t][k].load(Relaxed); }
}
/// Take from the task `t`'s ring the records from the cycle `since` and `before` records before
/// it, the old ones first (like `sched::strace_take`).
pub fn nw_ph_take(t: usize, since: u64, before: usize, out: &mut [PhEv]) -> usize {
    use core::sync::atomic::Ordering::Relaxed;
    if t >= crate::sched::NTASKS { return 0; }
    let end = NW_PH_POS[t].load(Relaxed) as usize;
    let start = end.saturating_sub(PH_N);
    let mut first = end;
    let mut k = start;
    while k < end {
        let e = unsafe { NW_PH[t][k % PH_N] };
        if e.tsc.wrapping_sub(since) < (1u64 << 62) { first = k; break; }
        k += 1;
    }
    let mut from = first.saturating_sub(before).max(start);
    let mut n = 0;
    while from < end && n < out.len() {
        out[n] = unsafe { NW_PH[t][from % PH_N] };
        n += 1; from += 1;
    }
    n
}
/// A FULL ANALYSIS OF ONE SLOW DELIVERY (>= `DL_DUMP_US`): who the reader is, in what
/// class, on which core, and ALL the decisions of its core's scheduler since the moment of the
/// delivery (see `sched::strace_note`). It is filled in `dl_close`, it is printed
/// by the `talaria.stat=` block (not from a syscall: printing under the stack's lock would cost
/// milliseconds to the whole machine) and the slot is given for the next one.
pub struct DlDump {
    pub used: core::sync::atomic::AtomicBool,
    pub t: u8, pub c: u8, pub cpu: u8, pub by: u8, pub occ: u8, pub ot: u8,
    pub us: u32, pub ran_us: u32, pub sw: u8, pub n: u8,
    pub dl: u64,
    pub ev: [crate::sched::SchedEv; DL_DUMP_EV],
    /// The phases of the reader and of the one that occupied its core (see `NW_PH`).
    pub pr: [PhEv; DL_DUMP_PH], pub po: [PhEv; DL_DUMP_PH], pub npr: u8, pub npo: u8,
    /// The occupant during the wait for the reader: us by phase and the outcomes of the yield window.
    pub opt: [u32; 16], pub owhy: [u32; WHY_N],
}
pub const DL_DUMP_PH: usize = 32;
pub const DL_DUMP_EV: usize = 40;
pub const DL_DUMP_N: usize = 6;
pub const DL_DUMP_US: u64 = 100;
const DLD0: DlDump = DlDump { used: core::sync::atomic::AtomicBool::new(false), t: 0, c: 0, cpu: 0, by: 0, occ: 0, ot: 255, us: 0, ran_us: 0, sw: 0, n: 0,
    dl: 0,
    ev: [crate::sched::SEV0; DL_DUMP_EV], pr: [PHE0; DL_DUMP_PH], po: [PHE0; DL_DUMP_PH], npr: 0, npo: 0, opt: [0; 16], owhy: [0; WHY_N] };
pub static mut DL_DUMP: [DlDump; DL_DUMP_N] = [DLD0; DL_DUMP_N];
/// How many slow deliveries did not fit into the slots (all of them were occupied).
pub static DL_DUMP_MISSED: AtomicU64 = AtomicU64::new(0);
/// How many analyses were skipped because of a flush of the block in progress (see dl_dump).
pub static DL_DUMP_SKIP_FLUSH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn dl_dump(i: usize, c: usize, us: u64, sw: u64, dl_at: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    let t = tt()[i].tid as usize;
    if t >= crate::sched::NTASKS { return; }
    // WHILE THE `stat:` BLOCK IS BEING POURED ONTO THE LINE — DO NOT WRITE. Session 32: the flush goes
    // in portions of 150 us from a tick every 2 ms (12 us per byte of the port), while the analysis's
    // slots are filled by the very first slow deliveries after the snapshot — that
    // is, exactly during the flush. All the analyses showed one and the same:
    // "a yield -> we came back after 127 us" — that was a portion of the flush in
    // the scheduler's prologue rather than the workload's tail. The instrument was catching itself.
    if crate::obs::flush_active() { DL_DUMP_SKIP_FLUSH.fetch_add(1, Relaxed); return; }
    for k in 0..DL_DUMP_N {
        let d = unsafe { &mut *core::ptr::addr_of_mut!(DL_DUMP[k]) };
        if d.used.load(Relaxed) { continue; }
        d.t = t as u8; d.c = c as u8; d.cpu = tt()[i].dlcpu; d.by = tt()[i].dlby; d.occ = tt()[i].dlo;
        d.ot = { let o = (tt()[i].dlx & 0xFFFF) as usize; if o < crate::sched::NTASKS { o as u8 } else { 255 } };
        d.us = us.min(u32::MAX as u64) as u32;
        d.ran_us = (crate::sched::run_ns(t).saturating_sub(tt()[i].dlr) / 1000).min(u32::MAX as u64) as u32;
        d.sw = sw.min(255) as u8;
        d.dl = dl_at;
        d.n = crate::sched::strace_take(d.cpu as usize, d.dl, 3, &mut d.ev) as u8;
        // The phases: from 30 us BEFORE the delivery, in order to see where the thread came from.
        let back = (unsafe { HZ } / 1_000_000).max(1) * 30;
        d.npr = nw_ph_take(t, d.dl.wrapping_sub(back), 1, &mut d.pr) as u8;
        d.npo = if d.ot != 255 && d.ot != d.t { nw_ph_take(d.ot as usize, d.dl.wrapping_sub(back), 1, &mut d.po) as u8 } else { 0 };
        d.opt = [0; 16]; d.owhy = [0; WHY_N];
        if d.ot != 255 && d.ot != d.t {
            let (mut pt, mut why) = ([0u64; 16], [0u32; WHY_N]);
            nw_snap(d.ot as usize, &mut pt, &mut why);
            let hz = unsafe { HZ }.max(1);
            for k in 0..16 { d.opt[k] = (pt[k].wrapping_sub(tt()[i].dlpt[k]).saturating_mul(1_000_000) / hz).min(u32::MAX as u64) as u32; }
            for k in 0..WHY_N { d.owhy[k] = why[k].wrapping_sub(tt()[i].dlwhy[k]); }
        }
        d.used.store(true, core::sync::atomic::Ordering::Release);
        return;
    }
    DL_DUMP_MISSED.fetch_add(1, Relaxed);
}
/// The data have been taken by the reader: close the delivery's interval of the slot `i`.
fn dl_close(i: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    let t0 = tt()[i].dl;
    if t0 == 0 { return; }
    tt()[i].dl = 0;
    let c = (tt()[i].dlc as usize).min(DLC_N - 1);
    let now = tsc::rdtsc();
    let us = now.wrapping_sub(t0).saturating_mul(1_000_000) / unsafe { HZ }.max(1);
    tt()[i].trd = t0; tt()[i].trrd = now; tt()[i].trdus = us.min(u32::MAX as u64) as u32;
    DL_HIST[c][dl_bucket(us)].fetch_add(1, Relaxed);
    DL_SUM_US[c].fetch_add(us, Relaxed);
    let x = tt()[i].dlx;
    if x & 0xFFFF == 0xFFFF { DL_IDLE[c].fetch_add(1, Relaxed); }
    let t = tt()[i].tid as usize;
    let sw = if t < crate::sched::NTASKS { crate::sched::cpu_switches(crate::sched::cpu_of(t)).wrapping_sub(x >> 16) } else { 0 };
    if us >= DL_SLOW_US && c != 0 && c != 6 {
        let o = tt()[i].dlo;
        DLS_OCC[c][(o & 3).min(2) as usize].fetch_add(1, Relaxed);
        DLS_SW[c][(sw as usize).min(3)].fetch_add(1, Relaxed);
        if o & 4 != 0 { DLS_HIDDEN[c].fetch_add(1, Relaxed); }
        let ot = (x & 0xFFFF) as usize;
        if ot < crate::sched::NTASKS { DLS_TASK[ot].fetch_add(1, Relaxed); }
        if t < crate::sched::NTASKS {
            let ran = crate::sched::run_ns(t).saturating_sub(tt()[i].dlr) / 1000; // us
            let share = ran.saturating_mul(100) / us.max(1);
            DLS_RAN[c][match share { 0..=9 => 0, 10..=49 => 1, 50..=89 => 2, _ => 3 }].fetch_add(1, Relaxed);
            let osw = crate::sched::switches(t).wrapping_sub(tt()[i].dls);
            DLS_OWNSW[c][(osw as usize).min(3)].fetch_add(1, Relaxed);
            let np = RD_PASS[t].load(Relaxed).wrapping_sub(tt()[i].dlp);
            DLS_PASS[c][match np { 0 => 0, 1..=2 => 1, 3..=10 => 2, _ => 3 }].fetch_add(1, Relaxed);
        }
        if us >= DL_DUMP_US && (c == 1 || c == 3) { dl_dump(i, c, us, sw, t0); }
    }
    if us > DL_MAX_US[c].load(Relaxed) {
        DL_MAX_US[c].store(us, Relaxed);
        DL_MAX_CTX[c].store((x & 0xFFFF) | (sw << 16), Relaxed);
    }
}
/// The pass of the stack started at `t_pass`, it brought frames; `since` is from what cycle
/// the ring could have been non-empty unnoticed.
fn dla_note(t_pass: u64, since: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    if since == 0 { return; }
    let us = t_pass.wrapping_sub(since).saturating_mul(1_000_000) / unsafe { HZ }.max(1);
    DLA_HIST[dl_bucket(us)].fetch_add(1, Relaxed);
    if us > DLA_MAX_US.load(Relaxed) { DLA_MAX_US.store(us, Relaxed); }
    LAST_DLA_US.store(us.min(u32::MAX as u64) as u32, Relaxed);
}
/// THE GAP BETWEEN THE PASSES WITH FRAMES (session 32). The generator sends its requests by
/// a grid (7566 req/s = 132 us), and on the server the passes with frames must go
/// by the same grid. A frame unnoticed in the ring for 100 us gives a pair of gaps of
/// ~232 and ~32 — regardless of any estimate "the ring->the pass", which is blind
/// to the sign's lie (a lie refreshes `RING_EMPTY_TSC`). Two lines: all the passes
/// and the passes by the timers' debt (`stale`), plus how many frames a pass brought.
pub const RXGAP_N: usize = 8;
pub static RXGAP_HIST: [[core::sync::atomic::AtomicU64; RXGAP_N]; 2] =
    [const { [const { core::sync::atomic::AtomicU64::new(0) }; RXGAP_N] }; 2];
pub static RXGAP_FRAMES: [core::sync::atomic::AtomicU64; 3] = [const { core::sync::atomic::AtomicU64::new(0) }; 3];
static LAST_RXPASS_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn rxgap_note(t_pass: u64, stale: bool, nrx: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    let prev = LAST_RXPASS_TSC.swap(t_pass, Relaxed);
    RXGAP_FRAMES[(nrx as usize).saturating_sub(1).min(2)].fetch_add(1, Relaxed);
    if prev == 0 { return; }
    let us = t_pass.wrapping_sub(prev) / (unsafe { HZ } / 1_000_000).max(1);
    let b = match us { 0..=49 => 0, 50..=99 => 1, 100..=149 => 2, 150..=199 => 3, 200..=249 => 4, 250..=299 => 5, 300..=499 => 6, _ => 7 };
    RXGAP_HIST[stale as usize][b].fetch_add(1, Relaxed);
}
/// The estimate "the ring->the pass" of the last pass with frames (for `Tcp.trdla`).
static LAST_DLA_US: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// The cycle of the last delivery of data over a socket — "the network is silent" for `talaria.statidle`.
pub static NET_LAST_DL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// ── THE SERVER'S TURNAROUND ON A SOCKET (session 32, step 1.1) ───────────────
//
// WHAT FOR. The stages separately (the delivery->the reader, the app's read->write, the write->
// the wait) each have their own p99, while the question is about the REQUEST's p99: adding
// percentiles says nothing. Here one request is counted from "the data landed
// in the socket" to "the answer was handed to the card" (the end of the `write`, after `poll_forced`) — with
// a breakdown by interval: (dla) the estimate "the ring->the pass" for the frame, (d)
// the delivery->the read, (a) the read->the write in ring 3, (l) the wait for the stack's lock in the write,
// (s) the sending under the lock. If the turnaround's p99 is much less than the request's p99, the tail
// is not in the server (the card/the hypervisor/the network); if it is equal — an interval is visible.
pub const TR_N: usize = 8;
/// The turnaround's buckets, us: <20, <40, <60, <80, <100, <150, <200, 200+ (X12:
/// the request's p99 sat between 50 and 100 — the bucket was too wide).
pub static TR_HIST: [AtomicU64; TR_N] = [const { AtomicU64::new(0) }; TR_N];
pub const TR_ST: usize = 5;
pub const TR_ST_NAMES: [&str; TR_ST] = ["the ring->the pass", "the delivery->the read", "the read->the write", "the lock in the write", "the sending"];
/// The sums of the intervals, us — over all the turnarounds and over the slow ones (>= 100 us).
pub static TR_SUM: [AtomicU64; TR_ST] = [const { AtomicU64::new(0) }; TR_ST];
pub static TR_SLOW_SUM: [AtomicU64; TR_ST] = [const { AtomicU64::new(0) }; TR_ST];
pub static TR_SLOW_N: AtomicU64 = AtomicU64::new(0);
/// The same for the AVERAGE turnarounds (60..99 us) — the p99 of a mysql pk request sits there.
pub static TR_MID_SUM: [AtomicU64; TR_ST] = [const { AtomicU64::new(0) }; TR_ST];
pub static TR_MID_N: AtomicU64 = AtomicU64::new(0);
pub static TR_MID_TOP: [AtomicU64; TR_ST] = [const { AtomicU64::new(0) }; TR_ST];
/// In the slow turnarounds — which interval was THE LARGEST.
pub static TR_SLOW_TOP: [AtomicU64; TR_ST] = [const { AtomicU64::new(0) }; TR_ST];
#[derive(Clone, Copy)]
pub struct TrEx { pub us: u32, pub st: [u32; TR_ST], pub t: u8, pub cpu: u8, pub by: u8, pub c: u8, pub ot: u16 }
pub const TR_EX_N: usize = 6;
pub static mut TR_EX: [TrEx; TR_EX_N] = [TrEx { us: 0, st: [0; TR_ST], t: 0, cpu: 0, by: 0, c: 0, ot: 0 }; TR_EX_N];
fn tr_bucket(us: u64) -> usize {
    match us { 0..=19 => 0, 20..=39 => 1, 40..=59 => 2, 60..=79 => 3, 80..=99 => 4, 100..=149 => 5, 150..=199 => 6, _ => 7 }
}
/// The end of the `write` on the socket `i`: `t_w0` is the entry into the write, `t_wl` is the lock taken.
fn tr_close(i: usize, t_w0: u64, t_wl: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    let t0 = tt()[i].trd;
    if t0 == 0 { return; }
    tt()[i].trd = 0;
    let hz = unsafe { HZ }.max(1);
    let us = |c: u64| c.saturating_mul(1_000_000) / hz;
    let now = tsc::rdtsc();
    let total = us(now.wrapping_sub(t0));
    // the estimate "the ring->the pass" is an upper one and is sometimes astronomical (the first
    // pass after a pause); no more than 10 ms is taken into the sum
    let st = [(tt()[i].trdla as u64).min(10_000), tt()[i].trdus as u64, us(t_w0.wrapping_sub(tt()[i].trrd)),
              us(t_wl.wrapping_sub(t_w0)), us(now.wrapping_sub(t_wl))];
    TR_HIST[tr_bucket(total)].fetch_add(1, Relaxed);
    for k in 0..TR_ST { TR_SUM[k].fetch_add(st[k], Relaxed); }
    if total < 60 { return; }
    if total < 100 {
        TR_MID_N.fetch_add(1, Relaxed);
        let mut top = 0;
        for k in 0..TR_ST { TR_MID_SUM[k].fetch_add(st[k], Relaxed); if st[k] > st[top] { top = k; } }
        TR_MID_TOP[top].fetch_add(1, Relaxed);
        return;
    }
    TR_SLOW_N.fetch_add(1, Relaxed);
    let mut top = 0;
    for k in 0..TR_ST { TR_SLOW_SUM[k].fetch_add(st[k], Relaxed); if st[k] > st[top] { top = k; } }
    TR_SLOW_TOP[top].fetch_add(1, Relaxed);
    // the worst examples (they are written under the stack's lock — the write holds it)
    let ex = unsafe { &mut *core::ptr::addr_of_mut!(TR_EX) };
    let mut m = 0;
    for k in 1..TR_EX_N { if ex[k].us < ex[m].us { m = k; } }
    if (ex[m].us as u64) < total {
        ex[m] = TrEx { us: total.min(u32::MAX as u64) as u32, st: core::array::from_fn(|k| st[k].min(u32::MAX as u64) as u32),
            t: tt()[i].tid, cpu: tt()[i].dlcpu, by: tt()[i].dlby, c: tt()[i].dlc, ot: (tt()[i].dlx & 0xFFFF) as u16 };
    }
}

/// Where the delivery went: to nobody / already to me / to a neighbour on this core / onto a foreign core.
/// The ratio answers the question "is there anybody to hand the processor over to at all".
pub static DELIV_NOOWNER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DELIV_SELF: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DELIV_SAMECPU: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DELIV_OTHERCPU: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// A DHCP client's event (a lease/a renewal/a loss) ON THE INTERFACE idx: apply
/// the address/gateway/DNS (musl's resolver reads /etc/resolv.conf — regenerate it).
/// The SNTP client from the tick (core 0): a workload that does not use the network does not drive
/// the stack, and the client in the pass never ran — one sample in 30 s on a Graviton. When a poll
/// is due and nobody has passed the stack for 2 ms, the tick drives it: a pass sends the burst's
/// query, and the reply is waited for by passes for at most 500 us (Amazon's round trip is 50) —
/// a reply taken a tick later would carry the tick in its round trip, half of it as an error.
/// A late reply ends the waiting for this tick; at most 2 ms of a tick in all. From a tick the
/// stack's lock is only TRIED (`net_enter`), as from the card's line.
pub fn ntp_tick() {
    if unsafe { CFG_NTP }.is_none() || !ready() || crate::percpu::cpu_id() != 0 { return; }
    let hz = unsafe { HZ };
    let st = unsafe { &*addr_of_mut!(NTP) };
    let t0 = tsc::rdtsc();
    if st.b.sent == 0 && t0 < st.next { return; }
    if t0.wrapping_sub(LAST_POLL_TSC.load(core::sync::atomic::Ordering::Relaxed)) < hz / 500 { return; }
    let (wait, cap) = (hz / 2000, hz / 500);
    loop {
        poll_forced();
        let st = unsafe { &*addr_of_mut!(NTP) };
        let now = tsc::rdtsc();
        if st.b.sent == 0 || now.wrapping_sub(t0) > cap { break; }
        if st.b.got < st.b.sent && now.wrapping_sub(st.last_tx) > wait { break; }
        core::hint::spin_loop();
    }
}

/// The SNTP client's step in the stack's pass: take the replies, finish a burst into one sample
/// for the servo, send the burst's next query when it is due. In the pass, under the stack's
/// lock, like the DHCP client — so it runs wherever the stack is driven, workload or idle.
fn ntp_process(idx: usize, ts: smoltcp::time::Instant) {
    use crate::ntp;
    let f = match ifopt(idx) { Some(f) => f, None => return };
    let h = match f.nt { Some(h) => h, None => return };
    let (srv, iv) = match unsafe { CFG_NTP } { Some(c) => c, None => return };
    let hz = unsafe { HZ };
    let st = unsafe { &mut *addr_of_mut!(NTP) };
    let mut buf = [0u8; 64];
    loop {
        let (len, from) = match f.sockets.get_mut::<udp::Socket>(h).recv() {
            Ok((d, m)) => { let l = d.len().min(buf.len()); buf[..l].copy_from_slice(&d[..l]); (l, m.endpoint) }
            Err(_) => break,
        };
        let t4 = tsc::rdtsc();
        if ep_parts(Some(from)).0 != srv { continue; }
        // A reply to a query of THIS burst only: a late one of the previous burst is stale.
        if let Some(s) = ntp::parse(&buf[..len]).filter(|r| r.cookie >= st.burst_t0 && st.b.sent > 0).and_then(|r| ntp::sample(&r, t4, hz)) {
            st.b.add(s);
        }
    }
    let now = tsc::rdtsc();
    if st.b.sent == 0 && now < st.next { return; }
    let gap = now.wrapping_sub(st.last_tx);
    let done = st.b.got >= ntp::BURST || (st.b.sent >= ntp::BURST && gap > hz / 2);
    if done {
        let iv_ns = iv as u64 * 1_000_000_000;
        match st.b.best {
            Some(s) if st.fed == 0 && st.boot => {
                if let Some(err) = crate::wallclock::set_base(s.ref_ns, s.at_tsc, crate::wallclock::SRC_NTP, iv_ns) {
                    st.fed = 1; st.silent = 0;
                    crate::println!("ntp: sample 1 from {}.{}.{}.{}: the error {:+} us, the round trip {} us (the best of {}/{}) — the boot's base is set onto it, not stepped",
                        srv[0], srv[1], srv[2], srv[3], err / 1000, s.rtt_ns / 1000, st.b.got, st.b.sent);
                }
            }
            Some(s) if crate::wallclock::feed_ref_iv(s.ref_ns, s.at_tsc, crate::wallclock::SRC_NTP, iv_ns) => {
                st.fed += 1; st.silent = 0;
                let (ppm, err, _, steps, back, _) = crate::tsc::wall_stats();
                // The first samples and every step are said; the steady state is `stat`'s.
                if st.fed <= 8 || (steps > 0 && err.unsigned_abs() > crate::clock::STEP_NS as u64) {
                    crate::println!("ntp: sample {} from {}.{}.{}.{}: the error {:+} us, the round trip {} us (the best of {}/{}), the servo {:+} ppm, steps {} back {}",
                        st.fed, srv[0], srv[1], srv[2], srv[3], err / 1000, s.rtt_ns / 1000, st.b.got, st.b.sent, ppm, steps, back);
                }
            }
            Some(_) => {}
            None => {
                st.silent += 1;
                if st.silent == 1 || st.silent % 64 == 0 {
                    crate::println!("ntp: no answer from {}.{}.{}.{} ({} poll(s) in a row)", srv[0], srv[1], srv[2], srv[3], st.silent);
                }
            }
        }
        st.b = ntp::Burst::new();
        st.next = now + iv as u64 * hz;
        return;
    }
    // The burst's next query: after the previous one's reply, or 200 ms without it.
    if st.b.sent < ntp::BURST && (st.b.sent == 0 || st.b.got >= st.b.sent || gap > hz / 5) {
        let t1 = tsc::rdtsc();
        if st.b.sent == 0 { st.burst_t0 = t1; }
        let q = ntp::request(t1);
        let dst = IpEndpoint::new(IpAddress::v4(srv[0], srv[1], srv[2], srv[3]), 123);
        if f.sockets.get_mut::<udp::Socket>(h).send_slice(&q, dst).is_ok() {
            st.b.sent += 1; st.last_tx = t1;
            f.iface.poll(ts, &mut f.dev, &mut f.sockets); // out in this same pass
            f.dev.flush();
        }
    }
}

fn dhcp_process(idx: usize) {
    let f = match ifopt(idx) { Some(f) => f, None => return };
    let dh = match f.dh { Some(h) => h, None => return };
    let (addr, router, dns) = match f.sockets.get_mut::<dhcpv4::Socket>(dh).poll() {
        None => return,
        Some(dhcpv4::Event::Deconfigured) => {
            // A loss of the lease (a NAK/an expiry): the address is removed, the client itself goes into
            // a re-DISCOVER. The starting Deconfigured (there has been no lease yet) goes silently.
            f.info.ip = [0; 4]; f.info.gw = [0; 4];
            f.iface.update_ip_addrs(|a| a.clear());
            crate::netcfg::del_routes_of(idx);
            if idx == 0 && unsafe { !CFG_DHCP_WAIT } {
                unsafe { CFG_DHCP_WAIT = true; }
                crate::println!("netsys: net{} dhcp deconfigured — a repeated discover", idx);
            }
            return;
        }
        Some(dhcpv4::Event::Configured(c)) => (c.address, c.router, c.dns_servers.first().copied()),
    };
    let ip = addr.address().octets();
    f.info.ip = ip; f.info.prefix = addr.prefix_len();
    f.info.gw = router.map_or([0; 4], |g| g.octets());
    apply_addr(idx);
    crate::netcfg::del_routes_of(idx);
    if let Some(gw) = router { crate::netcfg::add_route([0, 0, 0, 0], 0, gw.octets(), idx, 0); }
    unsafe {
        if idx == 0 { CFG_IP = ip; CFG_PREFIX = addr.prefix_len(); CFG_GW = f.info.gw; CFG_DHCP_WAIT = false; }
        if let Some(d) = dns { CFG_DNS = d.octets(); }
    }
    crate::fs::vf_init_net(unsafe { CFG_DNS }); // a fresh /etc/resolv.conf
    crate::println!("netsys: net{} dhcp lease {}.{}.{}.{}/{} gw={}.{}.{}.{} dns={}.{}.{}.{}",
        idx, ip[0], ip[1], ip[2], ip[3], addr.prefix_len(),
        f.info.gw[0], f.info.gw[1], f.info.gw[2], f.info.gw[3],
        unsafe { CFG_DNS[0] }, unsafe { CFG_DNS[1] }, unsafe { CFG_DNS[2] }, unsafe { CFG_DNS[3] });
}

fn s(slot: usize) -> &'static mut tcp::Socket<'static> {
    let t = tt()[slot];
    let f = ifr(t.ifx as usize);
    f.sockets.get_mut::<tcp::Socket>(t.h.expect("tcp handle"))
}
fn us(slot: usize) -> &'static mut udp::Socket<'static> {
    let u = uu()[slot];
    let f = ifr(u.ifx as usize);
    f.sockets.get_mut::<udp::Socket>(u.h.expect("udp handle"))
}
/// How often a waiter in a network syscall gives the core up to its neighbours (nanoseconds;
/// 0 — do not give it up at all, as it was). The knob `talaria.netyield=<us>`.
/// Five microseconds were chosen by a measurement on PostgreSQL (the ceiling and the p50 at
/// 0 / 2 / 5 / 10 / 20 / 100 us): at zero the p50 is 1148 us, at 2-10 us it is 66-77
/// us and the ceiling is 18-20 thousand req/s instead of 8.9 thousand, at 100 us it is worse again.
/// Between 2 and 10 there is no difference greater than the spread between runs — the
/// middle was taken.
pub static NETYIELD_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(5_000);
static LAST_YIELD: [core::sync::atomic::AtomicU64; crate::percpu::MAXCPU] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::percpu::MAXCPU];
pub fn netyield_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.netyield") {
        if let Ok(us) = v.parse::<u64>() {
            NETYIELD_NS.store(us * 1000, core::sync::atomic::Ordering::SeqCst);
            crate::println!("netsys: a waiter gives the core up not less often than once every {} us{}", us,
                if us == 0 { " (it is off)" } else { "" });
        }
    }
}

/// How many `pause`s a waiter spins between two passes of the stack when there is nobody
/// to give the core to (`yield_window`). The knob `talaria.netpause=<N>`.
///
/// WHY A KNOB. On EC2 a vCPU is an SMT thread: a network waiter spinning "the lock +
/// the scan of the ring + 32 pauses" (≈1.5 us of pause per ≈1 us of a pass)
/// shares a physical core with a neighbour that is computing a request right then. Linux in
/// `idle=poll` gives the resources to the SMT neighbour with the same `pause`, but WITHOUT a pass
/// of the stack between them. Session 30's hypothesis about mongodb's find losing by the p50
/// (1.64x with a win for redis/select1 — that is, the more expensive the request, the worse)
/// is checked by this knob directly: N=256 (≈12 us between the passes) should
/// remove the loss while adding not more than ≈6 us of waiting for a frame on average.
pub static NETPAUSE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(32);
pub fn netpause_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.netpause") {
        if let Ok(n) = v.parse::<u32>() {
            NETPAUSE.store(n, core::sync::atomic::Ordering::SeqCst);
            crate::println!("netsys: a waiter spins {} pauses between the passes of the stack", n);
        }
    }
}

/// Is there on the core `cpu` a thread that WAITS FOR THE NETWORK (the scheduler asks).
///
/// A network waiter is a thread that drives a pass of the stack and gives the core up to its neighbours
/// (`yield_window`). Until it gets the processor, NOBODY will see a frame:
/// interrupts from the card are not set up in this build, the stack is driven only from
/// the syscalls. Which means the frame's delivery latency is bounded from below not by the network but by
/// how long the COMPUTING neighbour holds the core — that is, by the slice. Exactly that is visible
/// in the measurement: the p50 is normal, the p99 equals the quantum.
///
/// The sign is "a yield from a network wait happened recently": it costs not a single
/// extra action on the hot path (the mark is set anyway) and goes out by itself
/// when the network load is gone.
pub fn poller_active(cpu: usize) -> bool {
    if cpu >= crate::percpu::MAXCPU { return false; }
    let last = LAST_YIELD[cpu].load(core::sync::atomic::Ordering::Relaxed);
    if last == 0 { return false; }
    let hz = unsafe { HZ };
    if hz == 0 { return false; }
    tsc::rdtsc().wrapping_sub(last) < hz / 500 // a window of 2 ms
}

/// Declare a thread the socket's owner: "I am the one waiting for this descriptor".
///
/// The owner is needed by the targeted yield (`ready_peer`): without it the core knows that
/// a frame has come but does not know WHOM to wake. It is set not at the socket's creation but
/// at the moment when a thread BLOCKS on it — a server is entitled to accept
/// a connection with one thread and to read with another (mongod does exactly that), and an owner
/// "by the accept" would point in the wrong direction.
fn claim_fd(fd: i64, tid: u8) {
    if let Some(i) = tcp_slot(fd) { tt()[i].tid = tid; return; }
    if let Some(i) = udp_slot(fd) { uu()[i].tid = tid; }
}

/// THE TASK HAS CLOSED ITS OWN COPY OF THE DESCRIPTOR — it is no longer a waiter for this socket.
///
/// The ownership lives exactly as long as the task has a descriptor. While this was not so,
/// a forking server stalled DEAD, and here is how: PostgreSQL's postmaster
/// accepts a connection (an `accept` makes IT the owner), forks
/// a backend and CLOSES its own copy — while the record "the owner" stays on it. After that
/// `ready_peer` honestly finds a socket WITH DATA whose "waiter" is
/// the postmaster, and gives the core to it; the postmaster gives it to the checkpointer, that one gives it
/// back, and the targeted yield closes into a ring between the two.
///
/// It was measured (the gate B10, one core, 30 connections): 35 tasks, all of them by all
/// the signs ready, while the counter of the scheduler's choices was
/// `10801170 10801157 511168 184397 184107 0 0 0 …` — the thirty forked
/// backends were not chosen A SINGLE TIME. The server did not reach `ready to accept
/// connections`, the connections piled up in CLOSE-WAIT, and that read as a leak of
/// sockets.
///
/// We deliberately do not take a lock here: one field of one byte is written, both
/// possible values are correct, while `close` is called including with the lock already
/// taken.
pub fn disown(fd: i64, tid: u8) {
    if !ready() { return; }
    if let Some(i) = tcp_slot(fd) {
        if tt()[i].tid == tid { tt()[i].tid = 255; DISOWNED.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
        return;
    }
    if let Some(i) = udp_slot(fd) {
        if uu()[i].tid == tid { uu()[i].tid = 255; DISOWNED.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
    }
}
/// How many times the ownership was removed by the closing of a descriptor. The gate looks exactly here:
/// "the server answered" on one connection passes on a broken kernel too, while
/// ZERO removals mean that the mechanism did not fire a single time.
pub static DISOWNED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// TO WHOM to give the processor when a waiter gives it up.
///
/// The yield was BLIND: a `yield_now` is a rotation, and the thread whose
/// frame came got the core not earlier than its turn reached it. With a server where
/// dozens of threads wait on the sockets (mongod, PostgreSQL's backends) that is exactly the
/// tail that is visible in the measurement: the p50 is normal, while the p99 equals the quantum.
///
/// We ask directly: is there a socket WITH DATA that ANOTHER thread waits for.
/// There is — we give the core TO IT (`yield_to` puts a hint to the scheduler), there is not —
/// the former rotation. That costs a walk over the socket table (32 TCP + 8 UDP) not
/// more often than once every `NETYIELD_NS` per core, that is, about once every five microseconds.
///
/// The stack's lock is taken BY AN ATTEMPT and is released BEFORE the yield: while it is held,
/// the scheduler does not preempt this core (`holds_lock`), and the yield would be
/// rejected silently.
///
/// ONE'S OWN CORE COMES FIRST. A hint to the scheduler works only on the core where
/// it was put; a thread from ANOTHER core will not come by it ("a foreign hint ->
/// a rotation"). Previously the first socket with data that turned up could belong to a foreign
/// thread, and the yield went as a rotation, although on our own core data were being waited for too. The second
/// element of the answer is "is it a foreign one": a foreign ready thread can be TAKEN by a waiter to itself
/// (`talaria.wakepull=1`, see `try_pull`).
///
/// THE OLDEST DATA COME FIRST. Previously the first socket with data was taken BY
/// THE TABLE's ORDER, and with four conn threads on one core the thread with the last
/// slot starved: session 32's analysis showed a chain of hints t11 -> t12 ->
/// t13 -> t11 -> … of 20 us per request, while t14's data lay for 160 us. The age
/// is the mark `dl` (the TSC cycle when the delivery saw the data in the socket).
fn ready_peer(me: u8, cpu: usize) -> Option<(usize, bool)> {
    let _g = match net_try() { Some(g) => g, None => {
        if (me as usize) < crate::sched::NTASKS { RP_BUSY[me as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
        return None;
    } };
    let mut same: Option<(usize, u64)> = None;
    let mut foreign: Option<(usize, u64)> = None;
    for i in 0..NS {
        let t = tt()[i];
        if t.k != K::Conn || t.h.is_none() || t.tid == 255 || t.tid == me { continue; }
        if !s(i).can_recv() { continue; }
        let (tid, age) = (t.tid as usize, t.dl);
        let slot = if crate::sched::cpu_of(tid) == cpu { &mut same } else { &mut foreign };
        if slot.is_none_or(|(_, a)| age < a) { *slot = Some((tid, age)); }
    }
    if let Some((t, _)) = same { return Some((t, false)); }
    for i in 0..NU {
        let u = uu()[i];
        if !u.used || u.h.is_none() || u.tid == 255 || u.tid == me { continue; }
        if !us(i).can_recv() { continue; }
        let tid = u.tid as usize;
        if crate::sched::cpu_of(tid) == cpu { return Some((tid, false)); }
        if foreign.is_none() { foreign = Some((tid, 0)); }
    }
    foreign.map(|(t, _)| (t, true))
}
/// TAKE A READY THREAD FROM A BUSY CORE. The knob `talaria.wakepull=1`.
///
/// The tail class "a low rate, a wakeup" (session 32, mysql pk at 7.5 thousand req/s:
/// p99 163 us against Linux's 83). The analysis of the slow deliveries showed one and the
/// same: the frame is delivered, the reader thread is READY, but its core is occupied by another thread in
/// ring 3 (a background thread of MariaDB after a rotational yield, or a neighbouring
/// conn thread stretched by preemptions) — and the reader waits for A TICK, 135-165 us. While on
/// a neighbouring core a network waiter spins meanwhile, having nobody to give the core to:
/// its scheduler does not understand a hint for a foreign thread.
///
/// Linux here does a select_idle_sibling: the woken thread is put on
/// a free core. With us a free core is a core with a spinner, and the spinner itself
/// sees whose data have come (`ready_peer`). Which means it is the one that moves the reader to
/// itself (`migrate_idle_task` — a safe protocol, it backs off if the scheduler
/// has already entered the task) and yields to it by address at once.
///
/// We do NOT take it if on the reader's core there is a network waiter too — that one will yield by itself not later
/// than after `NETYIELD_NS`, while a move costs a cold L2 — EXCEPT the case when
/// that core is overloaded: a pull itself drives the readers onto the core of an eternal spinner
/// (mariadbd's listener), and in the analysis four conn threads spun on one core
/// while the neighbouring one with one listener refused to take them ("on its core there is a network
/// waiter" — 173 thousand refusals against 110 thousand pulls). The rule is the same as at
/// a steal and at `wake_target`: a move must IMPROVE the balance of the ready ones.
pub static WAKEPULL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub static PULL_OK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PULL_LOST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PULL_SKIP_WAITER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PULL_SKIP_PIN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PULL_SKIP_RUN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The pulls "for the sake of the balance": on the reader's core there is a network waiter, but there are two more ready ones there.
pub static PULL_BAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn try_pull(t: usize, cpu: usize) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let c = crate::sched::cpu_of(t);
    if c == cpu { return true; }
    if crate::sched::is_running(t) { PULL_SKIP_RUN.fetch_add(1, Relaxed); return false; }
    if crate::affinity::is_pinned(t) || crate::affinity::allowed_mask(t) & (1u64 << cpu) == 0 {
        PULL_SKIP_PIN.fetch_add(1, Relaxed); return false;
    }
    let o = crate::percpu::cur_task_of(c);
    if o != u64::MAX && (o as usize) < crate::sched::NTASKS && NW_IN[o as usize].load(Relaxed) {
        let mut cnt = [0u8; crate::percpu::MAXCPU];
        crate::sched::runnable_counts(&mut cnt);
        if cnt[c] < cnt[cpu].saturating_add(2) { PULL_SKIP_WAITER.fetch_add(1, Relaxed); return false; }
        PULL_BAL.fetch_add(1, Relaxed);
    }
    if crate::sched::migrate_idle_task(t, cpu) { PULL_OK.fetch_add(1, Relaxed); true }
    else { PULL_LOST.fetch_add(1, Relaxed); false }
}
pub fn wakepull_from_cmdline(cmdline: &str) {
    if crate::cmdline::flag(cmdline, "talaria.wakepull") {
        WAKEPULL.store(true, core::sync::atomic::Ordering::SeqCst);
        crate::println!("netsys: a network waiter takes a ready thread from a busy core (talaria.wakepull=1)");
    }
    if crate::cmdline::flag(cmdline, "talaria.appprof") {
        crate::netprof::app_enable();
        crate::println!("netprof: the app stage per thread (read->write, on the core / off the core) — into the stat block");
    }
    if crate::cmdline::flag(cmdline, "talaria.wakeidle") {
        crate::affinity::WAKEIDLE.store(true, core::sync::atomic::Ordering::SeqCst);
        crate::println!("netsys: a core with a network waiter is free for a waking thread (talaria.wakeidle=1)");
    }
}
/// How many yields went BY ADDRESS (to the thread whose frame came) and how many into
/// the common rotation. The ratio answers the question "does the targeted handover work
/// at all": with a single-process workload it is zero by construction.
pub static YIELD_DIRECT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static YIELD_ROTATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// ── THE PARKING OF A WAITER ON A SOCKET (`talaria.netpark=<ns>`) ─────────────
///
/// WHAT FOR. A waiter in a `read`/`epoll_wait` POLLS the stack itself: 352-424 passes per
/// one turnaround of a request. Hence two measured troubles at once — the stage `the socket->the reader`
/// (p50 1.4 us with p99 182: a frame lies in the buffer until the owner reaches the
/// processor) and 278 takes of the stack's lock PER REQUEST (they held it for 9432 ms out of ~60
/// core-seconds). Neither is cured by "a better lock": there is nothing to divide the lock
/// with — `iface.poll` takes THE WHOLE socket set of the interface, and the interface is one.
///
/// It is cured by the waiter ceasing to drive the stack: it PARKS (becomes
/// invisible to the scheduler, the core goes to the neighbours or into a hlt), while it is woken by THE DELIVERY —
/// `deliver_ready` on the pass of the stack that brought the frame. While the pass is made by
/// the card line's handler (`nic_irq`), that is, the polling is replaced by an interrupt.
///
/// WHY THIS IS ONE CHANGE RATHER THAN TWO. The card's line WITHOUT the parking makes things worse
/// (measured: mongodb's p50 68.9 -> 107.7 us, p99 247.4 -> 325.4) — the handler
/// is added ON TOP of the polling rather than instead of it. The parking without the card's line is
/// impossible at all: with us the stack is driven only from the syscalls, and if all
/// the network threads sleep, NOBODY will parse a frame. So the parking is switched on
/// only together with a laid line, while its ceiling is an insurance against
/// a lost wakeup rather than a mechanism of delivery.
///
/// THE CEILING. `wait_on` goes with a COARSE deadline (the tick's accuracy): we are woken by
/// an interrupt rather than by a timer, and borrowing the core's LVT timer for an exact deadline at
/// every network wait would mean knocking that core's slice off (see `coarse` in
/// `sync::wait_on_ex`).
static NETPARK_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// ONE POLLS, THE REST SLEEP.
///
/// A network waiter by default does not park but DRIVES a pass of the stack: there are no interrupts from
/// the card in this build, and a frame is seen by the thread that waits for it. While the waiter
/// is one, that is correct and measured (session 22: the parking under virtio-mmio lost
/// to the polling). But with a server with a process PER CONNECTION there become as many waiters
/// as there are connections, and every one of them is A READY task: one core is divided among
/// all of them, and NOBODY reaches the end.
///
/// The measurement on which that was caught (PostgreSQL, the gate B10, one core): 36 tasks,
/// of them 35 in the READY state, all with the last syscall 281 (`epoll_pwait`),
/// 91 thousand switches a second and zero syscalls — that is, the machine is entirely
/// busy shuffling the waiters. 24 connections were accepted, 0 closed: the backends did not
/// reach their `exit`.
///
/// The rule: a pass of the stack is needed ONE per machine. The first waiter becomes
/// the polling one and works as before; the rest park on their own word
/// (the mechanism is the same as with `talaria.netpark=` — `wake_owner` wakes them by
/// the delivery) with a ceiling, so that a missed wakeup does not cost an eternity.
/// That way a single-threaded server (redis, nginx) notices nothing: it has one waiter
/// and it is the polling one.
static NET_WAITERS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static NET_POLLER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);
/// The ceiling of the sleep of a parked NON-polling one: a missed wakeup must not
/// turn into an eternity, while extra checks once every 5 ms cost nothing
/// against the background of the fact that it waits for the network anyway.
const NET_PARK_BACKSTOP_NS: u64 = 5_000_000;
/// How many times a waiter fell asleep as a NON-polling one (the diagnostics: it was for the sake of this number
/// that everything was done — on a single-threaded server it must stay a zero).
pub static NET_PARK_EXTRA: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// With what the sleep of a non-polling one ended: with a delivery or with the backstop.
///
/// SEPARATING THESE TWO NUMBERS IS OBLIGATORY. The mechanism claims "it is woken by the delivery", while
/// the 5 ms backstop is an insurance against a missed wakeup. While they were counted with one
/// number (`NET_PARK_EXTRA`), there was nothing to tell a working mechanism from a non-working one:
/// in both cases the waiter "fell asleep as a non-polling one", while the difference
/// between them was 29 times by the ceiling with MariaDB.
pub static NET_PARKX_WAKE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static NET_PARKX_TMO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The polling one left while the waiters stayed: the machine was left WITHOUT a pass of the stack.
///
/// And that is the suspect: the polling one's slot is taken for the duration of ONE
/// network syscall, and when its owner has waited for its frame and gone
/// to work, the parked neighbours learn nothing about it — they sleep, while
/// there is nobody to wake them, because nobody drives the stack any more.
pub static NET_POLLER_LEFT_BUSY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// One of the parked ones picked the pass of the stack up after the polling one left.
pub static NET_POLLER_HANDOFF: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// "A pass of the stack is one per machine" is switched on by the explicit knob `talaria.netsolo=1`.
static NETSOLO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Does anybody park at all: only then are the word's snapshot and the call needed.
#[inline]
fn park_on() -> bool {
    NETPARK_NS.load(core::sync::atomic::Ordering::Relaxed) != 0
        || NETSOLO.load(core::sync::atomic::Ordering::Relaxed)
        || NETMIX_NS.load(core::sync::atomic::Ordering::Relaxed) != 0
}

/// A MIXED SET MAY PARK TOO (`talaria.netmix=<ns>`, off by default).
///
/// A waiter whose set holds a pipe, an eventfd or a signalfd next to its sockets is not
/// allowed to park by `netsolo` — nothing moves its word when a non-socket becomes ready —
/// so it spins in `yield_window`. Every PostgreSQL process waits that way (a socket or two,
/// the latch's signalfd or self-pipe, the postmaster's death pipe): an idle server is six
/// tasks spinning `epoll_pwait`. Measured on arm64 under TCG with Alpine's PostgreSQL 16:
/// six tasks at ~32 % of a core each, 18 thousand switches a second, `select 1` taking from
/// zero to 147 s depending on how the spinners meet the backend; the x86 numbers were taken
/// with the spin, so the default stays.
///
/// With the knob a NON-polling waiter of a mixed set parks on its word like a socket-only one,
/// for at most this many nanoseconds (the ceiling also bounds how late a timerfd's expiry is
/// seen — nothing kicks for time). Sockets wake it by `wake_owner`; the producers of the
/// other readiness — a pipe's bytes, room or close (`proc::pipe_bump`), an eventfd's write,
/// a signal made pending (`proc::send`/`send_proc`, the signalfd's readiness) — call
/// `mix_kick`, which moves the word of every task inside a network wait. The order is
/// Dekker's: the waiter publishes `NW_IN` (SeqCst) before its snapshot of the word and its
/// scan; the producer publishes its state, then a SeqCst fence, then reads `NW_IN` — either
/// the producer sees the waiter and moves its word (the park returns at once), or the
/// waiter's scan sees the state.
static NETMIX_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static NET_MIX_PARKS: AtomicU64 = AtomicU64::new(0);
pub static NET_MIX_WAKE: AtomicU64 = AtomicU64::new(0);
pub static NET_MIX_TMO: AtomicU64 = AtomicU64::new(0);
pub static NET_MIX_KICKS: AtomicU64 = AtomicU64::new(0);
/// A non-socket's readiness has changed: wake the network waiters to rescan (see NETMIX_NS).
pub fn mix_kick() {
    use core::sync::atomic::Ordering::{Relaxed, SeqCst};
    if NETMIX_NS.load(Relaxed) == 0 { return; }
    core::sync::atomic::fence(SeqCst);
    if NET_WAITERS.load(SeqCst) == 0 { return; }
    for t in 0..crate::sched::NTASKS {
        if !NW_IN[t].load(SeqCst) { continue; }
        NW_WORD[t].fetch_add(1, SeqCst);
        if crate::sync::waiting_on(t) == nw_addr(t) {
            NET_MIX_KICKS.fetch_add(1, Relaxed);
            crate::sync::wake_addr(nw_addr(t), 1, crate::sched::proc_of(t), false);
        }
    }
}

/// The guard "I am waiting for the network": it counts the waiters and frees the polling one's slot.
pub struct NetWaiter(bool);
impl NetWaiter {
    pub fn new() -> Self {
        NET_WAITERS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        let me = crate::sched::current_id();
        // SeqCst: published before the wait's first snapshot of the word (`mix_kick`'s Dekker)
        if me < crate::sched::NTASKS { NW_IN[me].store(true, core::sync::atomic::Ordering::SeqCst); }
        crate::netprof::mark_wait_in(); // `talaria.appprof=1`: the stage write -> the network wait
        NetWaiter(false)
    }
    /// Become the polling one if the place is free.
    fn take_poll(&mut self) -> bool {
        if self.0 { return true; }
        let me = crate::sched::current_id();
        if NET_POLLER.compare_exchange(usize::MAX, me, core::sync::atomic::Ordering::SeqCst,
                                       core::sync::atomic::Ordering::SeqCst).is_ok() {
            self.0 = true;
        }
        self.0
    }
}
impl Drop for NetWaiter {
    fn drop(&mut self) {
        {
            let me = crate::sched::current_id();
            if me < crate::sched::NTASKS { NW_IN[me].store(false, core::sync::atomic::Ordering::Relaxed); }
        }
        if self.0 {
            let me = crate::sched::current_id();
            let _ = NET_POLLER.compare_exchange(me, usize::MAX, core::sync::atomic::Ordering::SeqCst,
                                                core::sync::atomic::Ordering::SeqCst);
            // THE POLLING ONE LEAVES WHILE THE WAITERS STAY. The slot is taken for the duration of
            // ONE network syscall: it waited for its frame — it returned the slot and
            // went to work. The parked neighbours learn nothing about that
            // (they sleep), and there becomes NOBODY to drive the stack until their backstop.
            // Here it is only counted — the analysis is in `wait_frame_w`.
            if NET_WAITERS.load(core::sync::atomic::Ordering::SeqCst) > 1 {
                NET_POLLER_LEFT_BUSY.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
        NET_WAITERS.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    }
}
/// The knob was set by hand — the default together with the card's line is not applied.
static NETPARK_SET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The parking's ceiling by default: a millisecond. That is NOT a mechanism of delivery (that
/// is done by an interrupt) but an insurance against a lost wakeup — and at the same time the
/// term within which the stack must get a pass for the sake of its own timers
/// (the TCP retransmits).
const NETPARK_DEFAULT_NS: u64 = 1_000_000;

/// Switch the parking on by default if the card's line is laid: without the line there is nobody
/// to drive the stack, and a sleeping waiter would cost the ceiling per frame.
pub fn netpark_setup() {
    use core::sync::atomic::Ordering::SeqCst;
    let line = NETIRQ_GSI.load(SeqCst) != u32::MAX;
    if !NETPARK_SET.load(SeqCst) {
        if line { NETPARK_NS.store(NETPARK_DEFAULT_NS, SeqCst); }
    }
    let ns = NETPARK_NS.load(SeqCst);
    if ns == 0 { return; }
    crate::println!("netsys: a waiter on a socket PARKS (the ceiling is {} us, the delivery wakes it{})",
        ns / 1000, if line { "" } else { ", THERE IS NO CARD LINE — the stack is driven only by the syscalls" });
}

/// The wakeup word PER THREAD: a delivery increases it at the socket's owner.
/// A separate word per thread rather than one common one — so that a frame wakes THE ONE it
/// came for rather than everybody sleeping on the network at once.
static NW_WORD: [core::sync::atomic::AtomicU32; crate::sched::NTASKS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; crate::sched::NTASKS];
/// How many times they parked, how many were woken by a delivery, how many left by
/// the ceiling. The third column is a direct check that the delivery wakes them: with
/// a healthy card line the ceiling almost never fires.
pub static NET_PARKS: AtomicU64 = AtomicU64::new(0);
pub static NET_PARK_WAKE: AtomicU64 = AtomicU64::new(0);
pub static NET_PARK_TMO: AtomicU64 = AtomicU64::new(0);
/// The wakeups sent by a delivery (the owner really was asleep).
pub static NET_DELIV_WAKE: AtomicU64 = AtomicU64::new(0);
/// THE CEILING, WHILE THE DATA WERE ALREADY THERE. It was for the sake of this number that the counter was introduced: "99 %
/// of the parkings ended with the ceiling" by itself means nothing — the ceiling
/// is fed by IDLE waiters (a thread without traffic parks and wakes up over the whole
/// run), and by a common counter they cannot be told from a loaded reader.
/// Here only the case that is a defect is counted: the waiter left
/// BY THE CEILING, and right after that its socket turned out to have data — which means
/// the delivery did not call the one whose frame came. Next to it — how many times the ceiling
/// was honest (there are still no data).
pub static NET_PARK_LATE: AtomicU64 = AtomicU64::new(0);
pub static NET_PARK_IDLE_TMO: AtomicU64 = AtomicU64::new(0);

#[inline] fn nw_addr(t: usize) -> u64 { crate::mem::vaddr(core::ptr::addr_of!(NW_WORD[t])) }

/// A snapshot of THIS thread's wakeup word. It is taken BEFORE the check of the socket's readiness:
/// a delivery that happened between the snapshot and the parking has already changed the word, and
/// the wait ends at once. Without that order the parking would be a one-way
/// ticket — exactly the race analysed at `sync::park_recheck`.
#[inline]
fn wake_seq() -> u32 {
    // The parking is off — the snapshot is needed by NOBODY, while the call costs a read of the task's
    // number and an atomic read at EVERY iteration of the waiting loop (tens of
    // millions per run). The default must stay exactly what it was.
    if !park_on() { return 0; }
    let t = crate::sched::current_id();
    if t >= crate::sched::NTASKS { return 0; }
    NW_WORD[t].load(core::sync::atomic::Ordering::SeqCst)
}

/// Wake the owner of the socket whose data have come.
///
/// The word moves ALWAYS (even if the owner is running right now): its snapshot could have
/// been taken before the delivery, and then the changed word is the only thing that
/// will keep the thread from parking on an already arrived frame. The call itself is made only
/// to a parked one: the check of `waiting_on` costs one read and removes the scan of
/// the task registry from the hot path.
fn wake_owner(tid: u8) {
    if !park_on() { return; } // nobody parks
    if tid == 255 { return; }
    let t = tid as usize;
    if t >= crate::sched::NTASKS { return; }
    NW_WORD[t].fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    if crate::sync::waiting_on(t) != nw_addr(t) { return; }
    NET_DELIV_WAKE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    crate::sync::wake_addr(nw_addr(t), 1, crate::sched::proc_of(t), false);
}

/// The wait for a frame: a parking if it is on, otherwise the former yield window.
///
/// `seq` is the snapshot from `wake_seq()`, taken BEFORE the readiness check.
/// `cap_ns` is how much is left to the waiter by ITS OWN timeout (0 — there is no timeout).
fn wait_frame_w(seq: u32, cap_ns: u64, w: &mut NetWaiter, only_sock: bool) -> bool {
    // A NEIGHBOUR IN A MUNMAP WAITS FOR OUR ACKNOWLEDGEMENT OF A TLB FLUSH.
    //
    // A network waiter is the most populated place of the kernel: with a server with a thread per
    // connection almost all the tasks stand here almost all the time, and the waiting path
    // goes with the interrupts forbidden (a call of the vector 0x42 will not reach us).
    // The flush's initiator waits for the acknowledgement for 10 ms and GIVES UP, leaving us with
    // a stale TLB entry — that is, the kernel keeps walking over a frame
    // that the allocator has already given to somebody else. Exactly that is visible in the stand's console:
    // `shootdown: core N did not acknowledge within 10 ms (the last syscall 281/202,
    // taken off by the host false)` — 957 times over a mongodb run and not once with mysql,
    // postgresql and redis. The price of the check is two atomic reads per entry into
    // the wait, and that is a price of integrity rather than of speed (the same reasoning
    // and the same line stand in `ext2::fs_lock` and in the spin of the stack's lock).
    crate::paging::tlb_service();
    // THERE IS MORE THAN ONE WAITER — A PASS OF THE STACK IS NEEDED BY ONLY ONE.
    //
    // The polling one works as before (see `wait_frame`), the rest sleep on
    // their own word: they will be woken by `wake_owner` at the delivery, while the 5 ms ceiling
    // insures against a missed wakeup. It was for the sake of this place that everything was done:
    // a single-threaded server has one waiter, it is the polling one, and the branch below is
    // not taken a single time.
    // THE SELF-SWITCHING-ON BY THE NUMBER OF WAITERS HAS BEEN REMOVED — A MEASUREMENT.
    //
    // It was here: `NETPARK_NS == 0 && NET_WAITERS >= 2` switched the parking on
    // for itself as soon as there was more than one waiter on the sockets.
    // With PostgreSQL (a process per connection) that helped, while with MariaDB (a THREAD
    // per connection) it cost 29 times: three waiters out of four fell asleep, and
    // they were woken not by `wake_owner` but by THE 5 ms BACKSTOP. In the suite that looked
    // like `select1` at 776 req/s against 22741 without the parking, a p50 of 2.6-3.1 ms with
    // a p99 of exactly 6 ms — that is, the backstop was the view on the graph.
    //
    // Now the rule "a pass of the stack is one per machine" lives behind the EXPLICIT knob
    // `talaria.netsolo=1` (the parking of the polling one is behind its own, `netpark=`),
    // and for the sleep of a non-polling one the OUTCOME is counted: a delivery or the backstop.
    // ONE MAY PARK ONLY BY SOCKETS — a non-polling one too.
    //
    // `epoll_wait` computes `only_sock` exactly for that, but the check stood
    // ONLY in the polling one's branch, while a non-polling one parked on a socket's
    // word with any set. The readiness of an eventfd, a timerfd, a pipe or a file does not move
    // the word — there would be nobody to wake such a sleeper, and the wait
    // would run into the backstop.
    if park_on() && only_sock && !w.take_poll() {
        let t = crate::sched::current_id();
        if t < crate::sched::NTASKS && !crate::console::in_tick() {
            let cap = if cap_ns == 0 { NET_PARK_BACKSTOP_NS } else { NET_PARK_BACKSTOP_NS.min(cap_ns) };
            NET_PARK_EXTRA.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let r = crate::sync::wait_on_coarse(NW_WORD[t].as_ptr(), seq, crate::tsc::mono_ns() + cap);
            if r == -110 { NET_PARKX_TMO.fetch_add(1, core::sync::atomic::Ordering::Relaxed); return true; }
            NET_PARKX_WAKE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return false;
        }
    }
    // A mixed set with `talaria.netmix=` (see NETMIX_NS): the non-polling one parks; the
    // polling one stays, as above, the stack's driver.
    let mix = NETMIX_NS.load(core::sync::atomic::Ordering::Relaxed);
    if mix != 0 && !only_sock && !w.take_poll() {
        let t = crate::sched::current_id();
        if t < crate::sched::NTASKS && !crate::console::in_tick() {
            let cap = if cap_ns == 0 { mix } else { mix.min(cap_ns) };
            NET_MIX_PARKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let r = crate::sync::wait_on_coarse(NW_WORD[t].as_ptr(), seq, crate::tsc::mono_ns() + cap);
            if r == -110 { NET_MIX_TMO.fetch_add(1, core::sync::atomic::Ordering::Relaxed); return true; }
            NET_MIX_WAKE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return false;
        }
    }
    // The polling one is as it was: by sockets it waits for a frame, with a mixed set it
    // yields the window (parking on a socket's word is not allowed, the readiness of a pipe or
    // an eventfd does not move it).
    if only_sock { wait_frame(seq, cap_ns) } else { yield_window(); false }
}

fn wait_frame(seq: u32, cap_ns: u64) -> bool {
    // THE SCHEDULER IS NOT REQUIRED HERE. The wait can do both lives: under
    // the scheduler a parked thread gives the core up to its neighbours (and sleeps in a hlt
    // when there are no neighbours), while in single-task mode it stays in place and waits for
    // the word — but IT DOES NOT DRIVE THE STACK MEANWHILE, and it is exactly a pass of the stack that we remove.
    let ns = NETPARK_NS.load(core::sync::atomic::Ordering::Relaxed);
    if ns == 0 {
        // THE PARKING IS OFF — THE PASS'S DEBT MUST BE PAID ALL THE SAME. `NIC_PENDING`
        // means: an interrupt came while the pass did not take place (the stack's lock was
        // busy), that is, the frame is already in the ring and parsed by nobody. Further down in the text
        // this check exists, but it is in the parking's branch, and with `talaria.netpark=0`
        // the debt was paid by nobody. On ENA that is deafness: after a delivery the device
        // holds the interrupt until the next unmask, while the unmask is done by the handler
        // that could not enter that time. The measurement of 7 September: with
        // `talaria.netirq=1 talaria.netpark=0` the phase B gives a zero on ALL the cases
        // (mysql: `Operation timed out`; mongodb: even a ping gives 0 req/s), with a live
        // server in the console.
        if NIC_PENDING.load(core::sync::atomic::Ordering::Acquire) {
            NIC_PENDING_RUN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            nw_ph(PH_NIC);
            poll_forced();
            return false;
        }
        yield_window();
        return false;
    }
    let t = crate::sched::current_id();
    if t >= crate::sched::NTASKS { yield_window(); return false; }
    // Parking from a tick is not allowed: a tick is a handler's body rather than a thread.
    if crate::console::in_tick() { yield_window(); return false; }
    // DO NOT SLEEP WITH AN UNPARSED RING. An interrupt whose pass did not take place because of
    // a busy lock is a frame that has already come; to fall asleep now means
    // to sleep through the ceiling for it (see NIC_PENDING).
    if NIC_PENDING.load(core::sync::atomic::Ordering::Acquire) {
        NIC_PENDING_RUN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        poll_forced();
        return false;                // the caller will recheck the readiness and decide anew
    }
    let cap = if cap_ns == 0 { ns } else { ns.min(cap_ns) };
    NET_PARKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    nw_ph(PH_PARK);
    let r = crate::sync::wait_on_coarse(NW_WORD[t].as_ptr(), seq, crate::tsc::mono_ns() + cap);
    if r == -110 { NET_PARK_TMO.fetch_add(1, core::sync::atomic::Ordering::Relaxed); return true; }
    NET_PARK_WAKE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    false
}
/// The waiter left by the ceiling — and does its socket have data? Then the delivery did not
/// call: that is exactly the defect for whose sake the counter was introduced.
#[inline]
fn note_cap_exit(had_data: bool) {
    if had_data { NET_PARK_LATE.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
    else { NET_PARK_IDLE_TMO.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
}

/// The window of yielding the processor to other threads between the iterations of a blocking wait.
#[inline] fn yield_window() {
    // §7.5: a sleep on the receive ring instead of spinning (talaria.netidle=1)
    if unsafe { CFG_NETIDLE } {
        if let Some(f) = ifopt(0) {
            if let Some((a, v)) = f.dev.idle_watch() {
                f.dev.arm_intr();                                   // without that nobody will wake the HLT
                unsafe { crate::sync::kwait_bounded(a, v, 200_000) } // the ceiling is 200 us
                return;
            }
        }
    }
    nw_ph(PH_WIN);
    if crate::syscall::THREADED.load(core::sync::atomic::Ordering::Relaxed) {
        // IS THERE ANYBODY TO GIVE THE CORE TO — AND IS IT NOT TOO OFTEN.
        //
        // A wait in an `epoll_wait` is a spin, and while it spins, the core is busy.
        // With one process (redis, nginx) there is nobody to give it to: the neighbour is not ready, and a spin
        // is the cheapest way to wait for a frame. But with a forking server
        // ALL the backends wait at once, and the single one whose frame came stands in the common
        // rotation: with a slice of 0.5 ms and five tasks on two cores it waits for its
        // turn for up to a millisecond. And that is the gap on the databases — measured with
        // `talaria.netprof=`: the delivery of a frame into ring 3 (`rx->app`) is 100 us with
        // PostgreSQL against 4 us with Redis on the same core and the same stack.
        //
        // GIVING IT UP AT EVERY ITERATION IS NOT ALLOWED — it was checked: five tasks, each
        // yielding immediately, give a storm of context switches (each has
        // its own address space, that is, a reload of CR3 and a loss of the TLB), and
        // PostgreSQL sags from 2000 to 150 requests a second. The spin here is not
        // an accident but a protection against that storm.
        //
        // So the yield IS BOUNDED IN TIME: not more often than once every `NETYIELD_NS` per
        // core. The threshold sets the price of the question directly: a waiter gives the core up not later
        // than after it, while there come out not more than one switch per that term.
        let ns = NETYIELD_NS.load(core::sync::atomic::Ordering::Relaxed);
        let me_t = crate::sched::current_id();
        if ns == 0 { nw_why(me_t, 1); }
        if ns != 0 {
            let cpu = crate::percpu::cpu_id();
            let pull = WAKEPULL.load(core::sync::atomic::Ordering::Relaxed);
            // With a pull the window is open even when there is nobody to give it up to on one's own core:
            // a ready thread may wait on A FOREIGN busy core (see try_pull).
            let other = cpu < crate::percpu::MAXCPU
                && crate::sched::cpu_has_other_runnable(cpu, crate::sched::current_id());
            if !(cpu < crate::percpu::MAXCPU && (other || pull)) { nw_why(me_t, 2); }
            if cpu < crate::percpu::MAXCPU && (other || pull) {
                let now = tsc::rdtsc();
                let need = (unsafe { HZ } / 1_000_000).max(1) * (ns / 1000).max(1);
                if now.wrapping_sub(LAST_YIELD[cpu].load(core::sync::atomic::Ordering::Relaxed)) >= need {
                    LAST_YIELD[cpu].store(now, core::sync::atomic::Ordering::Relaxed);
                    // BY ADDRESS, if there is somebody: the thread whose frame has already come
                    // gets the core at once rather than in its turn (see ready_peer).
                    let me = crate::sched::current_id() as u8;
                    nw_ph(PH_YIELD);
                    let busy0 = if me_t < crate::sched::NTASKS { RP_BUSY[me_t].load(core::sync::atomic::Ordering::Relaxed) } else { 0 };
                    crate::sched::yield_origin(crate::sched::YO_NET);
                    let rp0 = tsc::rdtsc();
                    let rp = ready_peer(me, cpu);
                    crate::sched::RP_DUR.note_cycles(tsc::rdtsc().wrapping_sub(rp0), cpu as u64 | (me as u64) << 8 | (rp.map_or(255, |(t, _)| t as u64)) << 16);
                    match rp {
                        Some((t, false)) => { nw_why(me_t, 7); YIELD_DIRECT.fetch_add(1, core::sync::atomic::Ordering::Relaxed); crate::sched::yield_to(t); }
                        Some((t, true)) if pull && try_pull(t, cpu) => {
                            nw_why(me_t, 7); YIELD_DIRECT.fetch_add(1, core::sync::atomic::Ordering::Relaxed); crate::sched::yield_to(t);
                        }
                        Some((t, true)) if other => {
                            // The former behaviour without a pull: a hint for a foreign
                            // thread, the scheduler will turn it into a rotation.
                            nw_why(me_t, 7); YIELD_DIRECT.fetch_add(1, core::sync::atomic::Ordering::Relaxed); crate::sched::yield_to(t);
                        }
                        None if other => { nw_why(me_t, 7); YIELD_ROTATE.fetch_add(1, core::sync::atomic::Ordering::Relaxed); crate::sched::yield_now(); }
                        None => {                   // on our own core there is nobody to give it to
                            let busy = me_t < crate::sched::NTASKS && RP_BUSY[me_t].load(core::sync::atomic::Ordering::Relaxed) != busy0;
                            nw_why(me_t, if busy { 4 } else { 5 });
                        }
                        Some(_) => { nw_why(me_t, 6); }
                    }
                    nw_ph(PH_BACK);
                    return;
                } else { nw_why(me_t, 3); }
            }
        }
        nw_ph(PH_SPIN);
        unsafe { crate::arch::cpu::irq_enable(); }
        crate::obs::stat_flush_spin(); // a byte of the `talaria.stat=` block — from here rather than from a tick
        let n = NETPAUSE.load(core::sync::atomic::Ordering::Relaxed);
        for _ in 0..n { core::hint::spin_loop(); }
        unsafe { crate::arch::cpu::irq_disable(); }
    } else { nw_why(crate::sched::current_id(), 0); core::hint::spin_loop(); }
}
/// The counter's rate for a waiter's TIMEOUT. `HZ` is set when the first card is added; a
/// process on a machine without a network (a test, a workload with no NIC) waits in
/// `epoll_wait`/`poll` all the same, and with `HZ`'s placeholder of 1 every timeout was zero
/// cycles — the wait returned 0 at once, a 5-second `epoll_wait` became a spin of calls
/// (both machines; `mixwait` found it).
fn timeout_hz() -> u64 { let h = unsafe { HZ }; if h > 1 { h } else { crate::tsc::hz().max(1000) } }
fn ms_to_cycles(ms: i64) -> u64 { (ms as u64) * (timeout_hz() / 1000) }

fn tcp_slot(fd: i64) -> Option<usize> {
    let i = (fd - TCP_BASE) as usize;
    if fd >= TCP_BASE && i < NS && tt()[i].k != K::Free && tt()[i].k != K::Backlog && tt()[i].k != K::Closing { Some(i) } else { None }
}
fn udp_slot(fd: i64) -> Option<usize> {
    let i = (fd - UDP_BASE) as usize;
    if fd >= UDP_BASE && i < NU && uu()[i].used { Some(i) } else { None }
}
pub fn is_tcp(fd: i64) -> bool { ready() && tcp_slot(fd).is_some() }
pub fn is_udp(fd: i64) -> bool { ready() && udp_slot(fd).is_some() }
pub fn is_epoll(fd: i64) -> bool { fd >= EP_BASE && ((fd - EP_BASE) as usize) < NE && unsafe { EP_USED[(fd - EP_BASE) as usize] } }
pub fn is_sock(fd: i64) -> bool { is_tcp(fd) || is_udp(fd) }

/// HOW MANY SOCKETS WERE ACCEPTED AND HOW MANY CLOSED. The difference answers the question
/// that is otherwise settled by going through hypotheses: "the slots have run out" is an honest
/// load or connections that nobody closed. The table is one per machine and
/// small (see `NS`), so a leak in it is visible not as a growth of memory but as
/// "the server has stopped accepting" — that is, as a breakage of the stand.
pub static ACCEPTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CLOSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The backlog slots that died BEFORE an accept() and were returned into Listen (see `reap_backlog`).
pub static BACKLOG_DEAD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Return into Listen the backlog slots that will NEVER be accepted.
///
/// A backlog slot is a smoltcp socket in Listen that after a handshake
/// becomes a connection and waits for a thread to reach an `accept4`. If
/// the peer closed it EARLIER (a FIN without data -> CloseWait, an RST -> Closed,
/// a broken handshake -> Closed), `accept4` does not take it (`may_recv` is false),
/// while `arm` counts it as brought up — and the slot occupied a place in the listener's backlog
/// FOREVER. After BACKLOG of those the listener silently stopped answering SYNs:
/// from the outside "the server died", in the console nothing (the slots have not run out, after all). On the stand
/// that read as "the stack stops accepting after ~4.6 thousand connections"
/// (the generator, not having waited for an answer, closed the connections before the accept);
/// locally eight connect+close without an accept are enough. The gate: `test.sh` filed.
///
/// Two phases, so as not to swallow the peer's RST: `abort()` moves the socket into
/// Closed with the addressee still known, the nearest pass of the stack sends the RST and
/// forgets the addressee, and only then does the socket `listen()` again.
fn reap_backlog() {
    for i in 0..NS {
        if tt()[i].k != K::Backlog || tt()[i].h.is_none() { continue; }
        let st = s(i).state();
        match st {
            State::Listen | State::SynReceived | State::Established => continue, // it is alive or still waiting
            _ if s(i).may_recv() => continue,           // there are data — an accept will return them, while a read will see an EOF
            State::Closed if s(i).remote_endpoint().is_some() => continue, // the RST has not left yet
            State::Closed => {
                let p = tt()[i].port;
                if s(i).listen(p).is_ok() {
                    tt()[i].armed = false;
                    BACKLOG_DEAD.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                } else { detach_tcp(i); tt()[i] = TCP0; note_slot_free(); } // it did not come up — we free it, `arm` will take a new one
            }
            _ => s(i).abort(),                          // CloseWait/Closing/LastAck/TimeWait -> an RST, then as with Closed
        }
    }
}

/// HOW MANY SLOTS ARE OCCUPIED AND BY WHAT — in one line of analysis.
///
/// "The slots have run out" is cured differently: a full backlog is the layout
/// of the listeners (how many of them there are and how many interfaces), a mountain of `Conn` is either
/// an honest load or connections that nobody closed, a mountain of `Closing` is
/// peers that do not answer a FIN. One number does not tell these cases apart, and
/// the analysis ran into going through hypotheses.
/// The order: occupied, listeners, backlog, connections, closed-but-not-untied,
/// created-but-not-bound.
pub fn census() -> (usize, usize, usize, usize, usize, usize) {
    let (mut used, mut lis, mut bl, mut conn, mut cl, mut unb) = (0, 0, 0, 0, 0, 0);
    for i in 0..NS {
        match tt()[i].k {
            K::Free => {}
            K::Listen => { used += 1; lis += 1; }
            K::Backlog => { used += 1; bl += 1; }
            K::Unbound => { used += 1; unb += 1; }
            K::Conn => { used += 1; conn += 1; }
            K::Closing => { used += 1; cl += 1; }
        }
    }
    (used, lis, bl, conn, cl, unb)
}
/// THE HIGH-WATER MARK of the socket table and how many times it ran into the ceiling.
///
/// The table's ceiling is the machine's ONLY hard limit on connections, and
/// there is nothing to reveal it by after the fact: by the moment a run is declared unsuccessful,
/// the slots have already been freed. The high-water mark answers the question "did we come close to the
/// ceiling at all" without catching the moment; the counter of the refusals answers "how many times we ran into it".
pub static SLOTS_HIGH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static SLOTS_NONE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Warn about the exhaustion of the socket table. Not more often than once every five seconds — but
/// NOT once per boot either: silence after the first time hid exactly what
/// one needs to know about production — is the machine holding at the ceiling or did it run into it
/// once. Every refusal is counted (`SLOTS_NONE`), one of them is printed.
fn warn_no_slots() {
    SLOTS_NONE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let now = tsc::rdtsc();
    let next = NEXT.load(core::sync::atomic::Ordering::Relaxed);
    if next != 0 && now.wrapping_sub(next) >= (1u64 << 62) { return; }   // it is too early yet
    NEXT.store(now.wrapping_add(ms_to_cycles(5000)), core::sync::atomic::Ordering::Relaxed);
    let (used, lis, bl, conn, cl, unb) = census();
    crate::println!("netsys: ATTENTION: the listener's backlog is NOT REPLENISHED — the TCP slots have run out ({} out of {} are occupied: listeners {}, \
        backlog {}, connections {}, closed but not untied {}, created but not bound {}); accepted over the run {}, closed {}, slot refusals {}. New \
        connections will HANG.",
        used, NS, lis, bl, conn, cl, unb,
        ACCEPTED.load(core::sync::atomic::Ordering::Relaxed),
        CLOSED.load(core::sync::atomic::Ordering::Relaxed),
        SLOTS_NONE.load(core::sync::atomic::Ordering::Relaxed));
}

/// A FREED SLOT IS AN EVENT FOR THE LISTENER rather than only for the one that
/// closed the socket.
///
/// `arm()` was called from exactly two places — `listen()` and `accept4()`. While there are
/// slots, that is enough: we accepted a connection — we replenished the backlog at once. But
/// as soon as the table runs into the ceiling ONCE, the listener stays WITHOUT
/// a backlog forever: there are no free slots at that moment, while a second occasion
/// to call `arm()` will never come — `accept4` is called only when
/// a ready connection already lies there, and there is nowhere for it to lie any more. After that the server
/// answers EVERY SYN with a refusal while staying alive and healthy in everything
/// else; the freeing of all the connections does not resurrect it.
///
/// Locally that reproduces in ten seconds (the gate `slotcap`): 80
/// keep-alive connections to `talaria.mode=eserver`, of which 63 come up, while
/// after their closing the server accepts NOT A SINGLE new one. On the EC2 stand the same
/// thing two runs in a row read as "after an overload the server stops
/// accepting connections" — without a single number in hand.
///
/// So: a slot that returned into `Free` raises a flag, while the nearest pass of the stack
/// replenishes all the listeners. The price is one atomic read per pass.
/// THIS MUST NOT BE ON THE HOT PATH. A pass of the stack is made per frame,
/// and a walk over the table in it is fractions of a percent of a core at a million frames a second.
/// So the mechanism sleeps until `arm()` has run into the ceiling at least once
/// (`STARVED`): in ordinary life that is one Relaxed load per freeing of a slot and
/// one per pass of the stack.
static STARVED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static NEED_ARM: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A slot has been freed. That interests the listener ONLY if it was starving.
fn note_slot_free() {
    if STARVED.load(core::sync::atomic::Ordering::Relaxed) {
        NEED_ARM.store(true, core::sync::atomic::Ordering::Relaxed);
    }
}
/// How many times the listeners had to be replenished AFTER running into the ceiling.
pub static REARMED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn rearm_listeners() {
    if !NEED_ARM.swap(false, core::sync::atomic::Ordering::Relaxed) { return; }
    REARMED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut full = true;
    for l in 0..NS {
        if tt()[l].k != K::Listen { continue; }
        arm(l);
        let mut have = 0;
        for i in 0..NS { if tt()[i].k == K::Backlog && tt()[i].owner == l as u8 { have += 1; } }
        if have < BACKLOG { full = false; }   // there were still not enough slots
    }
    // The starvation is over only when EVERY listener is replenished entirely.
    if full { STARVED.store(false, core::sync::atomic::Ordering::Relaxed); }
}

fn alloc_tcp() -> Option<usize> {
    // The occupied ones are counted ALONG THE WAY of the search for a free one: the walk is made
    // anyway, while the high-water mark cannot be taken without it.
    let mut free = None;
    let mut used = 0;
    for i in 0..NS {
        if tt()[i].k == K::Free && tt()[i].h.is_none() {
            if free.is_none() { free = Some(i); }
        } else { used += 1; }
    }
    let used = used as u64 + free.is_some() as u64;   // including the one that is about to be occupied
    SLOTS_HIGH.fetch_max(used, core::sync::atomic::Ordering::Relaxed);
    free
}
static mut EPH: u16 = 40000;
fn ephemeral() -> u16 {
    unsafe { EPH = if EPH >= 60000 { 40000 } else { EPH + 1 }; EPH }
}
fn ip_of(addr: *const u8) -> Option<(IpAddress, u16)> {
    if addr.is_null() { return None; }
    unsafe {
        let fam = core::ptr::read_unaligned(addr as *const u16);
        if fam != 2 { return None; } // only AF_INET
        let p = u16::from_be(core::ptr::read_unaligned(addr.add(2) as *const u16));
        let a = [*addr.add(4), *addr.add(5), *addr.add(6), *addr.add(7)];
        Some((IpAddress::v4(a[0], a[1], a[2], a[3]), p))
    }
}
fn write_sockaddr(dst: *mut u8, lenp: *mut u32, ep: Option<IpEndpoint>) {
    if dst.is_null() { return; }
    unsafe {
        core::ptr::write_bytes(dst, 0, 16);
        core::ptr::write_unaligned(dst as *mut u16, 2u16);
        if let Some(ep) = ep {
            core::ptr::write_unaligned(dst.add(2) as *mut u16, ep.port.to_be());
            let IpAddress::Ipv4(a) = ep.addr;
            let o = a.octets();
            core::ptr::copy_nonoverlapping(o.as_ptr(), dst.add(4), 4);
        }
        if !lenp.is_null() { core::ptr::write_unaligned(lenp, 16); }
    }
}

// ── the socket API (an fd>=0 or -errno) ───────────────────────────────────────
/// socket(domain, type, proto): SOCK_STREAM -> a TCP slot, SOCK_DGRAM -> a UDP slot.
pub fn socket(typ: u64) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    if !ready() { return -97; } // EAFNOSUPPORT
    let nb = typ & 0x800 != 0;
    if typ & 0xff == 2 {
        for i in 0..NU {
            if !uu()[i].used {
                uu()[i] = Udp { used: true, nonblock: nb, ..UDP0 };
                return UDP_BASE + i as i64;
            }
        }
        // §1.4: a silent EMFILE looks to musl's resolver like "the network does not
        // answer" — exactly the symptom of the known flake talaria.test=connect.
        crate::println!("netsys: the UDP sockets have run out ({} are occupied) -> EMFILE", NU);
        return EMFILE;
    }
    match alloc_tcp() {
        Some(i) => { tt()[i] = Tcp { k: K::Unbound, nonblock: nb, tid: crate::sched::current_id() as u8, ..TCP0 }; TCP_BASE + i as i64 }
        None => { crate::println!("netsys: the TCP sockets have run out ({} are occupied) -> EMFILE", NS); EMFILE }
    }
}
pub fn bind(fd: i64, addr: *const u8) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    // the port @+2 (BE) for AF_INET and AF_INET6 (a sockaddr_in6 — redis/the services bind "*" and ::)
    let port = if addr.is_null() { 0 } else { u16::from_be(unsafe { core::ptr::read_unaligned(addr.add(2) as *const u16) }) };
    if let Some(i) = tcp_slot(fd) {
        for j in 0..NS { if j != i && tt()[j].k == K::Listen && tt()[j].port == port && port != 0 { return EADDRINUSE; } }
        tt()[i].port = port; return 0;
    }
    if let Some(i) = udp_slot(fd) {
        let p = if port == 0 { ephemeral() } else { port };
        let ifx = match any_up() { Some(x) => x, None => return EADDRINUSE };
        if !attach_udp(i, ifx) { return EADDRINUSE; }
        return match us(i).bind(p) {
            Ok(_) => { uu()[i].bound = true; uu()[i].port = p; 0 }
            Err(e) => { crate::println!("netsys: the bind of UDP :{} was refused ({:?}) -> EADDRINUSE", p, e); detach_udp(i); EADDRINUSE }
        };
    }
    EBADF
}
/// Replenish the listener l's backlog: free slots go into Listen ON EVERY brought-up
/// interface. That way `listen()` still means "on all the node's addresses", although
/// a socket now belongs to a particular interface.
fn arm(l: usize) {
    reap_backlog();
    let p = tt()[l].port;
    let nif = unsafe { NIF };
    let mut nup = 0;
    for x in 0..nif { if if_up(x) { nup += 1; } }
    if nup == 0 { return; }
    let per = (BACKLOG / nup).max(2);
    for x in 0..nif {
        if !if_up(x) { continue; }
        let mut have = 0;
        for i in 0..NS { if tt()[i].k == K::Backlog && tt()[i].owner == l as u8 && tt()[i].ifx as usize == x { have += 1; } }
        while have < per {
            let i = match alloc_tcp() {
                Some(i) => i,
                // A SILENT REFUSAL HERE IS A HUNG CLIENT. Without replenishing
                // the backlog, the listener stops answering SYNs: a connection is not
                // rejected (that would be an honest error) but simply does not
                // happen, and from the outside it looks like "the server died". While
                // there was no line, the analysis cost an evening.
                None => { STARVED.store(true, core::sync::atomic::Ordering::Relaxed); warn_no_slots(); return; }
            };
            if !attach_tcp(i, x) { return; }
            if s(i).listen(p).is_err() { detach_tcp(i); return; }
            let h = tt()[i].h;
            tt()[i] = Tcp { k: K::Backlog, owner: l as u8, port: p, ifx: x as u8, h, ..TCP0 };
            have += 1;
        }
    }
}
pub fn listen(fd: i64) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    let i = match tcp_slot(fd) { Some(x) => x, None => return EBADF };
    if tt()[i].port == 0 { tt()[i].port = ephemeral(); }
    tt()[i].k = K::Listen;
    arm(i);
    0
}
/// accept4(fd, addr, len, flags): a ready backlog connection of the listener -> a new fd.
pub fn accept4(fd: i64, addr: *mut u8, lenp: *mut u32, flags: i64) -> i64 {
    // We hold the lock ONLY while we work with the stack: a blocking wait
    // with the lock clamped would stop the whole machine's network for the sake of one thread.
    let mut _g = match net_lock() { Some(g) => Some(g), None => return EAGAIN };
    let l = match tcp_slot(fd) { Some(x) => x, None => return EBADF };
    if tt()[l].k != K::Listen { return EINVAL };
    let nb_conn = flags & 0x800 != 0;
    if !tt()[l].nonblock { tt()[l].tid = crate::sched::current_id() as u8; } // I am the one waiting for connections (see claim_fd)
    let mut _w = NetWaiter::new();   // "I am waiting for the network" — see `wait_frame_w`
    loop {
        let seq = wake_seq(); // the snapshot BEFORE the check: see `wake_seq`
        poll();
        arm(l);
        for i in 0..NS {
            if tt()[i].k == K::Backlog && tt()[i].owner == l as u8 && tt()[i].h.is_some() && s(i).is_active() && s(i).may_recv() {
                tt()[i].k = K::Conn;
                ACCEPTED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                tt()[i].nonblock = nb_conn;
                tt()[i].armed = false;                          // it will be raised anew by this connection's data
                tt()[i].tid = crate::sched::current_id() as u8; // the accepting thread is the owner
                arm(l);
                write_sockaddr(addr, lenp, s(i).remote_endpoint());
                return TCP_BASE + i as i64;
            }
        }
        if tt()[l].nonblock { return EAGAIN; }
        if crate::proc::sig_pending() { return -4; } // EINTR
        _g = None;
        let by_cap = wait_frame_w(seq, 0, &mut _w, true);
        _g = net_lock();
        // For an accepting one "the data in hand" is a ready connection in the
        // backlog: we left by the ceiling while it was already lying there.
        if by_cap {
            let ready = (0..NS).any(|i| tt()[i].k == K::Backlog && tt()[i].owner == l as u8
                && tt()[i].h.is_some() && s(i).is_active() && s(i).may_recv());
            note_cap_exit(ready);
        }
    }
}
/// connect(fd, sockaddr): TCP — an active open (blocking/EINPROGRESS); UDP — remember the addressee.
pub fn connect(fd: i64, addr: *const u8) -> i64 {
    // We hold the lock ONLY while we work with the stack: a blocking wait
    // with the lock clamped would stop the whole machine's network for the sake of one thread.
    let mut _g = match net_lock() { Some(g) => Some(g), None => return EAGAIN };
    let (ip, port) = match ip_of(addr) { Some(x) => x, None => return EINVAL };
    let dst = match ip { IpAddress::Ipv4(a) => a.octets() };
    if let Some(i) = udp_slot(fd) {
        // The interface for UDP is chosen by the same routing table: a socket that sends
        // to an address of the management network must go into the management card.
        let ifx = crate::netcfg::egress(dst).unwrap_or(uu()[i].ifx as usize);
        if !uu()[i].bound || uu()[i].ifx as usize != ifx {
            let p = if uu()[i].port != 0 { uu()[i].port } else { ephemeral() };
            if !attach_udp(i, ifx) { return EADDRINUSE; }
            if us(i).bind(p).is_err() { return EADDRINUSE; }
            uu()[i].bound = true; uu()[i].port = p;
        }
        uu()[i].remote = Some(IpEndpoint::new(ip, port));
        return 0;
    }
    let i = match tcp_slot(fd) { Some(x) => x, None => return EBADF };
    let t = tt()[i];
    if t.k == K::Conn {
        if t.connecting { return if s(i).state() == State::Established { tt()[i].connecting = false; 0 } else { -114 /* EALREADY */ }; }
        return EISCONN;
    }
    if t.k != K::Unbound { return EINVAL; }
    let ifx = match crate::netcfg::egress(dst).or_else(any_up) { Some(x) => x, None => return -101 }; // ENETUNREACH
    if !attach_tcp(i, ifx) { return -101; }
    let lport = if t.port != 0 { t.port } else { ephemeral() };
    let f = ifr(ifx);
    let sk = f.sockets.get_mut::<tcp::Socket>(tt()[i].h.unwrap());
    sk.set_nagle_enabled(false); // the latency: without Nagle (latency clients set TCP_NODELAY anyway)
    sk.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
    if sk.connect(f.iface.context(), IpEndpoint::new(ip, port), IpListenEndpoint { addr: None, port: lport }).is_err() {
        detach_tcp(i);
        return EINVAL;
    }
    tt()[i].k = K::Conn;
    tt()[i].connecting = true;
    if trace_on() {
        let (a, p) = ep_parts(Some(IpEndpoint::new(ip, port)));
        crate::println!("net: connect fd={} -> {}.{}.{}.{}:{} (local :{}, net{}, {})", fd, a[0], a[1], a[2], a[3], p, lport, ifx,
            if tt()[i].nonblock { "nonblock" } else { "blocking" });
    }
    poll_forced();                       // the SYN leaves in this pass
    if tt()[i].nonblock { return EINPROGRESS; }
    let t0 = tsc::rdtsc();
    let limit = ms_to_cycles(10_000);
    loop {
        poll();
        match s(i).state() {
            State::Established => { tt()[i].connecting = false; if trace_on() { crate::println!("net: fd={} established", fd); } return 0; }
            State::Closed => { tt()[i].connecting = false; tt()[i].err = ECONNREFUSED as i32; if trace_on() { crate::println!("net: fd={} refused", fd); }
                return ECONNREFUSED; }
            _ => {}
        }
        if tsc::rdtsc() - t0 > limit { s(i).abort(); tt()[i].connecting = false; if trace_on() { crate::println!("net: fd={} connect timeout", fd); }
            return ETIMEDOUT; }
        _g = None; yield_window(); _g = net_lock();
    }
}
/// Update the status of a non-blocking connect by the socket's state.
fn refresh_conn(i: usize) {
    if tt()[i].connecting {
        match s(i).state() {
            State::Established => { tt()[i].connecting = false; if trace_on() { crate::println!("net: fd={} established (async)", TCP_BASE + i as i64); } }
            State::Closed => { tt()[i].connecting = false; tt()[i].err = ECONNREFUSED as i32; if trace_on() { crate::println!("net: fd={} refused (async)",
                TCP_BASE + i as i64); } }
            _ => {}
        }
    }
}
/// recv(fd, buf, len, flags): MSG_DONTWAIT=0x40, MSG_PEEK=2.
pub fn read(fd: i64, buf: *mut u8, len: usize, flags: u64) -> i64 {
    // We hold the lock ONLY while we work with the stack: a blocking wait
    // with the lock clamped would stop the whole machine's network for the sake of one thread.
    let mut _g = match net_lock() { Some(g) => Some(g), None => return EAGAIN };
    let i = match tcp_slot(fd) { Some(x) => x, None => return EBADF };
    if tt()[i].k != K::Conn { return ENOTCONN; }
    let nb = tt()[i].nonblock || flags & 0x40 != 0;
    if !nb { tt()[i].tid = crate::sched::current_id() as u8; } // I am the one that will wait (see claim_fd)
    let mut _w = NetWaiter::new();   // "I am waiting for the network" — see `wait_frame_w`
    let mut first = true;
    nw_ph(PH_READ);
    loop {
        let seq = wake_seq(); // the snapshot BEFORE the check: see `wake_seq`
        if !first || !s(i).can_recv() {
            nw_ph(PH_POLL);
            poll();
            let me = crate::sched::current_id();
            if me < crate::sched::NTASKS { RD_PASS[me].fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
        }
        first = false;
        refresh_conn(i);
        if tt()[i].err != 0 { let e = tt()[i].err; tt()[i].err = 0; return e as i64; }
        let sk = s(i);
        if sk.can_recv() {
            let dst = unsafe { core::slice::from_raw_parts_mut(buf, len) };
            if flags & 2 != 0 { return sk.peek_slice(dst).map(|n| n as i64).unwrap_or(0); }
            let n = sk.recv_slice(dst).map(|n| n as i64).unwrap_or(0);
            tt()[i].rx += n as u64;
            crate::netprof::mark_deliv(tt()[i].rdy); // §7.1: how long the frame waited for THE READER
            tt()[i].rdy = 0;
            dl_close(i);
            tt()[i].armed = false; // we took them — the next appearance of data calls the owner again
            crate::netprof::mark_app_in(); // §7.1
            return n;
        }
        if !sk.may_recv() { return 0; } // EOF: the connection is closed by the peer
        if nb { return EAGAIN; }
        if crate::proc::sig_pending() { return -4; } // EINTR
        _g = None;
        let by_cap = wait_frame_w(seq, 0, &mut _w, true);
        _g = net_lock();
        // The defect is visible exactly here: we left by the ceiling while the data are ALREADY there —
        // which means the delivery did not call the one whose frame came.
        if by_cap { note_cap_exit(tcp_slot(fd).is_some_and(|j| s(j).can_recv())); }
    }
}
pub fn write(fd: i64, buf: *const u8, len: usize, flags: u64) -> i64 {
    // We hold the lock ONLY while we work with the stack: a blocking wait
    // with the lock clamped would stop the whole machine's network for the sake of one thread.
    nw_ph(PH_WRITE);
    crate::netprof::mark_app_out(); // §7.1: the boundary is the ENTRY into the write, before the stack's lock
    let t_w0 = tsc::rdtsc();
    let mut _g = match net_lock() { Some(g) => Some(g), None => return EAGAIN };
    let t_wl = tsc::rdtsc();
    let i = match tcp_slot(fd) { Some(x) => x, None => return EBADF };
    if tt()[i].k != K::Conn { return ENOTCONN; }
    // A deferred error (a refusal of the connection) goes as the FIRST answer to a send, as
    // in Linux. Without that an application that writes without asking SO_ERROR
    // got an EPIPE and declared "the peer closed", although there was no connection
    // at all; in the stand's analysis those are two different reasons.
    refresh_conn(i);
    if tt()[i].err != 0 { let e = tt()[i].err; tt()[i].err = 0; return e as i64; }
    let nb = tt()[i].nonblock || flags & 0x40 != 0;
    let src = unsafe { core::slice::from_raw_parts(buf, len) };
    let mut off = 0;
    // THE FIRST pass of the stack before the send is NOT needed. The room in the transmit buffer is
    // the socket's local state, there is no point in driving the whole interface for it, while
    // the application gets here right after an epoll_wait, which has just driven the
    // stack. Previously `write` made TWO full passes and TWO calls to the card per
    // every call; under a hypervisor a call is a VMEXIT, and the syscalls' profile
    // showed 12.2 us for a write against 0.4 us for a read with a request's turnaround of
    // 36 us. If there is no room — a drive is made at the next turn of the loop.
    let mut first = true;
    while off < len {
        if !first { poll(); }
        first = false;
        refresh_conn(i);
        let sk = s(i);
        if sk.can_send() {
            match sk.send_slice(&src[off..]) { Ok(n) => { off += n; tt()[i].tx += n as u64; }, Err(_) => break }
        } else if !sk.may_send() { if off == 0 { return -32; } break; } // EPIPE
        else if nb { if off > 0 { break; } return EAGAIN; }
        else { _g = None; yield_window(); _g = net_lock(); }
    }
    // THIS PASS IS THE SENDING — it must not be throttled.
    //
    // `pollgap` throttles the IDLE passes: those where nothing came and there is nothing
    // to do. A pass after a `send_slice` is never idle — in it a frame goes
    // into the transmit ring and rings the device. Throttled, it defers
    // the sending until somebody's next pass, and with a waiter's parking that
    // is no longer microseconds: the echo's measurement showed the stage `app->tx` at 873 us with
    // a parking ceiling of 1 ms — the answer lay ready and waited for somebody
    // to drive the stack.
    poll_forced();
    tr_close(i, t_w0, t_wl);
    off as i64
}
pub fn shutdown(fd: i64, _how: i64) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    let i = match tcp_slot(fd) { Some(x) => x, None => return EBADF };
    if tt()[i].k == K::Conn { s(i).close(); poll_forced(); } // the FIN leaves in this pass
    0
}
pub fn close(fd: i64) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    if let Some(i) = udp_slot(fd) {
        if uu()[i].h.is_some() { us(i).close(); }
        detach_udp(i);
        uu()[i] = UDP0;
        return 0;
    }
    let i = match tcp_slot(fd) { Some(x) => x, None => return 0 };
    let k = tt()[i].k;
    if k == K::Listen {
        // CLOSING A LISTENER IS AN EVENT RATHER THAN A ROUTINE, and it must name
        // itself. After it the machine stops answering SYNs at all (not with a refusal
        // — with silence: there is no socket on the port any more), and from the outside that is indistinguishable
        // from a disappearance of the network. Exactly like that looked postgresql's failure on the stand
        // of 8 September: `listeners 0` in the slots' layout, while WHO closed it and WHEN
        // there was nothing to learn by. There are never many lines here:
        // a workload has a handful of listeners over its whole life.
        let port = tt()[i].port;
        crate::println!("netsys: the listener :{} IS CLOSED (fd={}, the task {}, accepted over the run {}, closed {})",
            port, fd, crate::sched::current_id(),
            ACCEPTED.load(core::sync::atomic::Ordering::Relaxed),
            CLOSED.load(core::sync::atomic::Ordering::Relaxed));
        for j in 0..NS { if tt()[j].k == K::Backlog && tt()[j].owner == i as u8 { abort_slot(j); } }
        detach_tcp(i);
        tt()[i] = TCP0; note_slot_free(); poll_forced(); return 0;
    }
    if k == K::Conn {
        let t = tt()[i];
        TOT_RX.fetch_add(t.rx, core::sync::atomic::Ordering::Relaxed); TOT_TX.fetch_add(t.tx, core::sync::atomic::Ordering::Relaxed);
        let tr = if trace_on() { Some(ep_parts(s(i).remote_endpoint())) } else { None };
        CLOSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if tt()[i].h.is_some() && !matches!(s(i).state(), State::Closed | State::TimeWait) {
            s(i).close(); // a FIN — while its fate will be awaited by a pass of the stack rather than by this thread
            tt()[i].k = K::Closing; tt()[i].tid = 255; tt()[i].armed = false;
            let now = tsc::rdtsc();
            tt()[i].rdy = now.wrapping_add(ms_to_cycles(ORPHAN_IDLE_MS));
            tt()[i].rx = now.wrapping_add(ms_to_cycles(ORPHAN_CAP_MS));
            tt()[i].tx = s(i).send_queue() as u64;
            ORPHANS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            poll_forced(); // the FIN leaves in this pass
        } else {
            detach_tcp(i);
            tt()[i] = TCP0; note_slot_free();
        }
        drop(_g); // the trace goes OUTSIDE the lock: printing into the serial costs milliseconds
        if let Some((a, p)) = tr { crate::println!("net: close fd={} {}.{}.{}.{}:{} rx={} tx={}", fd, a[0], a[1], a[2], a[3], p, t.rx, t.tx); }
        return 0;
    }
    detach_tcp(i);
    tt()[i] = TCP0; note_slot_free();
    0
}
/// The orphans in the table (see `K::Closing`) — so that a pass of the stack does not walk 64 slots in vain.
static ORPHANS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// The orphans whose handshake did not fit into the term and got an RST.
pub static ORPHAN_RST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Bring the orphans to the end: an untied one (Closed without an addressee or TIME-WAIT)
/// is freed; an overdue one gets an `abort()` — the RST will leave with the nearest
/// pass, after which smoltcp will forget the addressee and the slot will be freed right here.
fn reap_orphans() {
    if ORPHANS.load(core::sync::atomic::Ordering::Relaxed) == 0 { return; }
    let now = tsc::rdtsc();
    for i in 0..NS {
        if tt()[i].k != K::Closing { continue; }
        let done = match tt()[i].h {
            None => true,
            Some(_) => match s(i).state() {
                State::TimeWait => true,
                State::Closed => s(i).remote_endpoint().is_none(),
                _ => {
                    if tt()[i].rdy != 0 {
                        // The send queue has decreased — the peer is alive and reading: the term of silence starts anew.
                        let q = s(i).send_queue() as u64;
                        if q < tt()[i].tx { tt()[i].tx = q; tt()[i].rdy = now.wrapping_add(ms_to_cycles(ORPHAN_IDLE_MS)); }
                        let idle_out = now.wrapping_sub(tt()[i].rdy) < (1u64 << 62);
                        let cap_out = now.wrapping_sub(tt()[i].rx) < (1u64 << 62);
                        if idle_out || cap_out {
                            s(i).abort(); tt()[i].rdy = 0; // the term has expired: an RST, then as with Closed
                            ORPHAN_RST.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    false
                }
            },
        };
        if done {
            detach_tcp(i); tt()[i] = TCP0; note_slot_free();
            ORPHANS.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
}
pub fn fcntl(fd: i64, cmd: i64, arg: i64) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    let nbp: Option<&mut bool> = if let Some(i) = tcp_slot(fd) { Some(&mut tt()[i].nonblock) }
        else if let Some(i) = udp_slot(fd) { Some(&mut uu()[i].nonblock) } else { None };
    match nbp {
        Some(nb) => match cmd {
            4 => { *nb = arg & 0x800 != 0; 0 } // F_SETFL
            3 => if *nb { 0x800 | 2 } else { 2 },  // F_GETFL (O_RDWR)
            _ => 0,
        },
        None => 0,
    }
}
/// setsockopt(level, opt): TCP_NODELAY (6,1) -> Nagle off; SO_* -> ok.
pub fn setsockopt(fd: i64, level: i64, opt: i64, _val: *const u8) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    if let Some(i) = tcp_slot(fd) {
        if level == 6 && opt == 1 { tt()[i].nodelay = true; if tt()[i].h.is_some() { s(i).set_nagle_enabled(false); } }
        return 0;
    }
    if udp_slot(fd).is_some() { return 0; }
    0
}
/// getsockopt: SO_ERROR (1,4) -> the deferred error (a non-blocking connect); the rest is 0.
pub fn getsockopt(fd: i64, level: i64, opt: i64, val: *mut u8, lenp: *mut u32) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    let mut v: i32 = 0;
    if let Some(i) = tcp_slot(fd) {
        if tt()[i].h.is_some() { refresh_conn(i); }
        // SO_ERROR is returned POSITIVE (that is how libc reads it: `errno = err`),
        // while in the slot the error lies NEGATIVE — exactly in the form in which it is
        // returned by the syscall (a `read` returns `err` as it is). While the sign was stored
        // the other way round, both consumers lied AT ONCE: `getsockopt` wrote into SO_ERROR
        // a -111, and musl printed "No error information" instead of "Connection
        // refused" (on the stand that is how mongod's refusal looked), while a `read` returned
        // a +111 — that is, "111 bytes have been read" from a socket that was refused.
        if level == 1 && opt == 4 { v = -tt()[i].err; tt()[i].err = 0; }
        else if level == 6 && opt == 1 { v = tt()[i].nodelay as i32; }
        else if level == 1 && opt == 3 { v = 1; } // SO_TYPE = SOCK_STREAM
    } else if udp_slot(fd).is_some() && level == 1 && opt == 3 { v = 2; }
    if !val.is_null() { unsafe { core::ptr::write_unaligned(val as *mut i32, v); } }
    if !lenp.is_null() { unsafe { core::ptr::write_unaligned(lenp, 4); } }
    0
}
pub fn getsockname(fd: i64, addr: *mut u8, lenp: *mut u32) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    if let Some(i) = tcp_slot(fd) {
        let ep = if tt()[i].h.is_some() { s(i).local_endpoint() } else { None }.or_else(|| {
            let ip = cfg_ip(); Some(IpEndpoint::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), tt()[i].port)) });
        write_sockaddr(addr, lenp, ep); return 0;
    }
    if let Some(i) = udp_slot(fd) {
        let ip = if_info(uu()[i].ifx as usize).map_or(cfg_ip(), |f| f.ip);
        let ep = IpEndpoint::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), uu()[i].port);
        write_sockaddr(addr, lenp, Some(ep)); return 0;
    }
    EBADF
}
pub fn getpeername(fd: i64, addr: *mut u8, lenp: *mut u32) -> i64 {
    // A refusal happens only from a tick (the console on top of the workload): there we do not
    // touch the stack and honestly say "it is busy" rather than "it is ready, 0 bytes".
    let _g = match net_lock() { Some(g) => g, None => return EAGAIN };
    if let Some(i) = tcp_slot(fd) {
        if tt()[i].h.is_none() { return ENOTCONN; }
        match s(i).remote_endpoint() { Some(ep) => { write_sockaddr(addr, lenp, Some(ep)); 0 }, None => ENOTCONN }
    } else if let Some(i) = udp_slot(fd) {
        match uu()[i].remote { Some(ep) => { write_sockaddr(addr, lenp, Some(ep)); 0 }, None => ENOTCONN }
    } else { EBADF }
}

// ── readiness / epoll / poll ──────────────────────────────────────────────────
pub const EPOLLIN: u32 = 0x001;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLET: u32 = 0x8000_0000;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLRDHUP: u32 = 0x2000;
/// The readiness mask of an fd (a socket). None — it is not a socket (the caller decides).
pub fn readiness(fd: i64) -> Option<u32> {
    let _g = match net_lock() { Some(g) => g, None => return Default::default() };
    if let Some(i) = tcp_slot(fd) {
        let t = tt()[i];
        if t.k == K::Listen {
            for j in 0..NS {
                if tt()[j].k == K::Backlog && tt()[j].owner == i as u8 && tt()[j].h.is_some() && s(j).is_active() && s(j).may_recv() { return Some(EPOLLIN); }
            }
            return Some(0);
        }
        if t.k != K::Conn || t.h.is_none() { return Some(0); }
        refresh_conn(i);
        let t = tt()[i];
        let sk = s(i);
        let mut m = 0;
        if t.err != 0 { m |= EPOLLERR | EPOLLHUP | EPOLLOUT | EPOLLIN; return Some(m); }
        if t.connecting { return Some(0); }
        if sk.can_recv() { m |= EPOLLIN; }
        if !sk.may_recv() { m |= EPOLLIN | EPOLLRDHUP; }          // EOF
        if !sk.may_send() && !sk.may_recv() { m |= EPOLLHUP; }
        if sk.can_send() { m |= EPOLLOUT; }
        return Some(m);
    }
    if let Some(i) = udp_slot(fd) {
        if uu()[i].h.is_none() { return Some(EPOLLOUT); }
        let sk = us(i);
        let mut m = 0;
        if sk.can_recv() { m |= EPOLLIN; }
        if sk.can_send() { m |= EPOLLOUT; }
        return Some(m);
    }
    None
}

const EPN: usize = 64;
static mut EP_USED: [bool; NE] = [false; NE];
static mut EP_FD: [[i64; EPN]; NE] = [[0; EPN]; NE];
static mut EP_EVENTS: [[u32; EPN]; NE] = [[0; EPN]; NE];
static mut EP_DATA: [[u64; EPN]; NE] = [[0; EPN]; NE];
static mut EP_ON: [[bool; EPN]; NE] = [[false; EPN]; NE];
// EPOLLET by an eventfd/timerfd: the generation the waiter has ALREADY been told about.
// The sockets stay by level (the reader scoops out until an EAGAIN, the level and
// the edge coincide), while an eventfd by level is a storm: asio does NOT read
// its eventfd interrupter (it relies on the edge), and mongod spun an
// epoll_pwait a million times a second, taking the stack's lock at every one
// ("the lock is contended 57 %", a whole core in vain).
static mut EP_ETGEN: [[u64; EPN]; NE] = [[0; EPN]; NE];
pub fn epoll_create() -> i64 {
    unsafe {
        for e in 0..NE {
            if !EP_USED[e] { EP_USED[e] = true; for i in 0..EPN { EP_ON[e][i] = false; } return EP_BASE + e as i64; }
        }
    }
    EMFILE
}
pub fn epoll_close(epfd: i64) -> i64 { if is_epoll(epfd) { unsafe { EP_USED[(epfd - EP_BASE) as usize] = false; } } 0 }
/// A closed fd leaves ALL the epoll sets — as in Linux, where a descriptor
/// leaves the interest lists together with the last reference to the file description.
///
/// Without that a record outlived its fd, while the fd numbers are reused: on
/// a server with keep-alive nginx got an `EPOLL_CTL_ADD -> EEXIST` on a FRESHLY
/// accepted connection and tore it down without answering. From the outside that looks like
/// "the server sometimes closes a connection silently" and does not reproduce on the first
/// requests — the numbers have not gone round the second time yet.
pub fn epoll_forget(fd: i64) {
    if is_epoll(fd) { return; }
    unsafe {
        for e in 0..NE {
            if !EP_USED[e] { continue; }
            for i in 0..EPN { if EP_ON[e][i] && EP_FD[e][i] == fd { EP_ON[e][i] = false; } }
        }
    }
}
pub fn epoll_ctl(epfd: i64, op: i64, fd: i64, event: *const u8) -> i64 {
    if !is_epoll(epfd) { return EBADF; }
    let e = (epfd - EP_BASE) as usize;
    // epoll_event {u32 events @0; u64 data @4} packed
    let (ev, data) = if event.is_null() { (0u32, 0u64) } else {
        unsafe { (core::ptr::read_unaligned(event as *const u32), core::ptr::read_unaligned(event.add(4) as *const u64)) }
    };
    unsafe {
        match op {
            1 | 3 => {
                let mut slot = None;
                for i in 0..EPN { if EP_ON[e][i] && EP_FD[e][i] == fd { slot = Some(i); break; } }
                if op == 1 && slot.is_some() { return -17; } // EEXIST
                if slot.is_none() { for i in 0..EPN { if !EP_ON[e][i] { slot = Some(i); break; } } }
                if let Some(i) = slot { EP_ON[e][i] = true; EP_FD[e][i] = fd; EP_EVENTS[e][i] = ev; EP_DATA[e][i] = data; EP_ETGEN[e][i] = 0; 0 }
                else { -28 } // ENOSPC
            }
            2 => { for i in 0..EPN { if EP_ON[e][i] && EP_FD[e][i] == fd { EP_ON[e][i] = false; } } 0 }
            _ => EINVAL,
        }
    }
}
/// The readiness of an arbitrary fd for epoll/poll: the sockets by the stack; null/eventfd/pipe —
/// only OUT; the files — IN|OUT; an epoll fd — none.
pub fn readiness_any(fd: i64) -> u32 { any_readiness(fd) }
fn any_readiness(fd: i64) -> u32 {
    if ready() { if let Some(m) = readiness(fd) { return m; } }
    if (300..1400).contains(&fd) { return EPOLLIN | EPOLLOUT; }         // a file from the disk
    if crate::proc::pipe_is(fd) { return crate::proc::pipe_readiness(fd); } // a real pipe
    if (2000..2256).contains(&fd) { return crate::syscall::nullfd_readiness(fd); } // a null fd: an eventfd/timerfd by its state, the rest OUT
    if (2256..2400).contains(&fd) { return EPOLLOUT; }                  // shm
    if fd == 0 { return 0; }
    if fd == 1 || fd == 2 { return EPOLLOUT; }
    0
}
pub fn epoll_wait(epfd: i64, events: *mut u8, maxev: i64, timeout: i64) -> i64 {
    if !is_epoll(epfd) { return EBADF; }
    let e = (epfd - EP_BASE) as usize;
    let mut _w = NetWaiter::new();   // "I am waiting for the network" — see `wait_frame_w`
    let mut gate = ScanGate::new();  // the scan of the set under the lock — not at every iteration
    let t0 = tsc::rdtsc();
    let limit = if timeout > 0 { ms_to_cycles(timeout) } else { 0 };
    let mut only_sock = true;        // it survives the rejected iterations (see ScanGate)
    loop {
        let seq = wake_seq(); // the snapshot BEFORE the scan of the set: see `wake_seq`
        if ready() { poll(); }
        let mut n: i64 = 0;
        if gate.need() {
        // ONE MAY PARK ONLY BY SOCKETS. In a set there are sometimes descriptors
        // that the network does not concern at all (an eventfd, a timerfd, pipes, files), and
        // their readiness comes not from the card — there would be nobody to wake a sleeper,
        // and the wait would run into the parking's ceiling. So the parking is allowed
        // only when THE WHOLE set is sockets.
        only_sock = true;
        // The stack's lock is ONE for the whole scan of the set rather than per descriptor.
        // `readiness` takes it itself, and on sixty-four slots that came out as
        // sixty-four pairs of CASes per iteration of the wait — on a server's hot loop
        // (an epoll_wait spins between the requests) that is a noticeable share of a
        // turnaround. The lock has recursion, so the nested entries are now
        // cheap.
        let _scan = net_lock();
        let me = crate::sched::current_id() as u8;
        for i in 0..EPN {
            if n >= maxev { break; }
            unsafe {
                if !EP_ON[e][i] { continue; }
                if !is_sock(EP_FD[e][i]) { only_sock = false; }
                claim_fd(EP_FD[e][i], me); // I am the one waiting for this descriptor (see claim_fd)
                let want = EP_EVENTS[e][i] | EPOLLERR | EPOLLHUP;
                let mut m = any_readiness(EP_FD[e][i]) & want;
                if m & EPOLLIN != 0 && EP_EVENTS[e][i] & EPOLLET != 0 && EP_FD[e][i] >= 2000 && EP_FD[e][i] < 2256 {
                    // the edge has already been given to this set — by level we do not repeat it any more
                    let g = crate::syscall::nullfd_gen(EP_FD[e][i]);
                    if g != 0 { if g == EP_ETGEN[e][i] { m &= !EPOLLIN; } else { EP_ETGEN[e][i] = g; } }
                }
                if m != 0 {
                    let dst = events.add((n as usize) * 12);
                    core::ptr::write_unaligned(dst as *mut u32, m);
                    core::ptr::write_unaligned(dst.add(4) as *mut u64, EP_DATA[e][i]);
                    n += 1;
                }
            }
        }
        drop(_scan);              // waiting with the lock clamped is not allowed
        gate.done(only_sock);
        }
        if n > 0 { nw_ph(PH_RET); return n; }
        if timeout == 0 { return 0; }
        if timeout > 0 && tsc::rdtsc() - t0 > limit { return 0; }
        if crate::proc::sig_pending() { return -4; } // EINTR
        // we do not hold the lock — it is taken by readiness()/poll()
        let by_cap = wait_frame_w(seq, left_ns(t0, limit), &mut _w, only_sock);
        // The same accounting as at a blocking read: we left by the ceiling, while the set
        // already has a ready descriptor — which means the delivery did not call.
        if by_cap {
            let ready_now = unsafe {
                let _s2 = net_lock();
                (0..EPN).any(|i| EP_ON[e][i] && any_readiness(EP_FD[e][i]) & (EP_EVENTS[e][i] | EPOLLERR | EPOLLHUP) != 0)
            };
            note_cap_exit(ready_now);
        }
    }
}
// ── THE REJECTION OF THE SET'S SCAN WITHOUT THE LOCK (`talaria.netscan=<ns>`) ─
//
// WHAT WAS MEASURED. `diag28-mysql2`, mysql/select1: the stack's lock was taken 991 572 968
// times, 38 % with a wait, in total 332 808 ms of pure spin — over 5 880 022
// requests. That is 169 takes of the MACHINE's lock per one request and about 14 %
// of the whole machine's processor spent on a cache line.
//
// WHERE THEY COME FROM. A waiter in a `poll(2)`/`epoll_wait` SPINS a loop, and every iteration
// takes the lock for the scan of the set of descriptors. Between two frames there are
// hundreds of such iterations, and all of them give one and the same answer "there are no ready ones".
//
// THE REJECTION. A socket's readiness is changed only by a pass of the stack, and that is globally visible by
// the counter of the received frames (`l1::rx_seen`). It has not moved — the set could not have become
// ready, and the scan under the lock is not needed. The counter is read WITHOUT the lock.
//
// THREE CONDITIONS UNDER WHICH THE REJECTION IS FORBIDDEN, and every one of them was bought with a defect:
//   * the first iteration — the owner must be declared (`claim_fd`) and what is
//     already ready must be seen;
//   * the set has a NON-SOCKET (an eventfd, a timerfd, a pipe, a file) — their readiness
//     comes past the card, and `rx_seen` knows nothing about it;
//   * a backstop by time — for the case of an event that we did not foresee
//     (a break, an orphan, a freed slot). A missed event costs not
//     an eternity but this threshold.
/// The rejection's backstop: a scan under the lock is made not less often than once every that much.
/// Twenty microseconds rather than a hundred: a hundred is exactly the length of the plateau we
/// are curing, and buying it back for nothing would be ridiculous.
static NETSCAN_NS: AtomicU64 = AtomicU64::new(20_000);
/// How many scans of the set were rejected without the lock (and how many were made).
pub static SCAN_SKIPPED: AtomicU64 = AtomicU64::new(0);
pub static SCAN_DONE: AtomicU64 = AtomicU64::new(0);

pub fn netscan_from_cmdline(cmdline: &str) {
    if let Some(ns) = crate::cmdline::num(cmdline, "talaria.netscan") {
        NETSCAN_NS.store(ns, core::sync::atomic::Ordering::SeqCst);
        crate::println!("netsys: a scan of the set under the lock not less often than once every {} ns{}", ns,
            if ns == 0 { " (the rejection is off — as it was)" } else { "" });
    }
}

/// The state of the rejection for ONE waiting loop.
struct ScanGate { rx: u64, tsc: u64, sock_only: bool, first: bool }
impl ScanGate {
    fn new() -> Self { ScanGate { rx: 0, tsc: 0, sock_only: false, first: true } }
    /// Is a scan of the set under the lock needed right now.
    fn need(&mut self) -> bool {
        use core::sync::atomic::Ordering::Relaxed;
        if self.first || !self.sock_only { return true; }
        let ns = NETSCAN_NS.load(Relaxed);
        if ns == 0 { return true; }
        if crate::l1::rx_seen() != self.rx { return true; }
        let hz = unsafe { HZ };
        if hz == 0 { return true; }
        let need = (hz / 1_000_000).max(1) * (ns / 1000).max(1);
        if tsc::rdtsc().wrapping_sub(self.tsc) >= need { return true; }
        SCAN_SKIPPED.fetch_add(1, Relaxed);
        false
    }
    /// The scan took place: remember the marks and whether the whole set is sockets.
    fn done(&mut self, only_sock: bool) {
        use core::sync::atomic::Ordering::Relaxed;
        SCAN_DONE.fetch_add(1, Relaxed);
        self.rx = crate::l1::rx_seen();
        self.tsc = tsc::rdtsc();
        self.sock_only = only_sock;
        self.first = false;
    }
}

/// HOW MUCH IS LEFT to the waiter by ITS OWN timeout, in nanoseconds (0 — there is no timeout).
/// The parking is not entitled to sleep through the application's timeout: an `epoll_wait(…, 5)` must
/// return after five milliseconds rather than after the parking's ceiling.
fn left_ns(t0: u64, limit: u64) -> u64 {
    if limit == 0 { return 0; }
    let spent = tsc::rdtsc().wrapping_sub(t0);
    if spent >= limit { return 1; }
    (limit - spent).saturating_mul(1_000_000_000) / timeout_hz()
}
/// poll(fds, nfds, timeout_ms): struct pollfd {i32 fd; i16 events; i16 revents}.
pub fn poll_fds(fds: *mut u8, nfds: usize, timeout: i64) -> i64 {
    const POLLIN: u16 = 1; const POLLOUT: u16 = 4; const POLLERR: u16 = 8; const POLLHUP: u16 = 0x10; const POLLNVAL: u16 = 0x20;
    let mut _w = NetWaiter::new();   // "I am waiting for the network" — see `wait_frame_w`
    let mut gate = ScanGate::new();  // the scan of the set under the lock — not at every iteration
    let t0 = tsc::rdtsc();
    let limit = if timeout > 0 { ms_to_cycles(timeout) } else { 0 };
    let mut only_sock = true; // see epoll_wait: one may park only by sockets
    nw_ph(PH_ENTER);
    loop {
        nw_ph(PH_TOP);
        let seq = wake_seq(); // the snapshot BEFORE the scan of the set: see `wake_seq`
        if ready() { nw_ph(PH_POLL); poll(); }
        let mut n: i64 = 0;
        if gate.need() {
        only_sock = true;
        nw_ph(PH_LOCK);
        // THE LOCK IS FOR THE WHOLE SCAN — both for the sake of `claim_fd` and for the same reason as
        // in `epoll_wait`: `readiness` takes it itself, and on a dozen descriptors
        // that came out as ten pairs of CASes per iteration of the hot loop.
        let _scan = net_lock();
        nw_ph(PH_SCAN);
        let me = crate::sched::current_id() as u8;
        for k in 0..nfds {
            unsafe {
                let p = fds.add(k * 8);
                let fd = core::ptr::read_unaligned(p as *const i32) as i64;
                let ev = core::ptr::read_unaligned(p.add(4) as *const u16);
                let mut rev: u16 = 0;
                if fd < 0 { core::ptr::write_unaligned(p.add(6) as *mut u16, 0); continue; }
                if !is_sock(fd) { only_sock = false; }
                // I AM THE ONE WAITING FOR THIS DESCRIPTOR — exactly the same as `epoll_wait` does.
                //
                // Without this line the socket's owner stayed the thread that
                // ACCEPTED it (`accept`), and the delivery called it rather than the one that
                // stands on the socket. With a server with a thread per connection waiting in
                // a `poll` (MariaDB does exactly that — 10050 calls of #7 over 10061
                // requests) that meant that NOBODY wakes a parked one:
                // 26141 fallings asleep, of them 4 woken by a delivery, the rest by
                // the 5 ms backstop. The ceiling of `select1` meanwhile was 821 req/s
                // against 38350; the p50 2.7 ms, the p99 5.9 ms — the backstop's plateau.
                claim_fd(fd, me);
                let m = any_readiness(fd);
                if fd >= 3 && m == 0 && !is_sock(fd) && !is_epoll(fd) && !(300..2500).contains(&fd) { rev |= POLLNVAL; }
                if m & EPOLLIN != 0 && ev & POLLIN != 0 { rev |= POLLIN; }
                if m & EPOLLOUT != 0 && ev & POLLOUT != 0 { rev |= POLLOUT; }
                if m & EPOLLERR != 0 { rev |= POLLERR; }
                if m & EPOLLHUP != 0 { rev |= POLLHUP; }
                core::ptr::write_unaligned(p.add(6) as *mut u16, rev);
                if rev != 0 { n += 1; }
            }
        }
        drop(_scan);              // waiting with the lock clamped is not allowed
        gate.done(only_sock);
        }
        if n > 0 { nw_ph(PH_RET); return n; }
        if timeout == 0 { return 0; }
        if timeout > 0 && tsc::rdtsc() - t0 > limit { return 0; }
        if crate::proc::sig_pending() { return -4; } // EINTR
        // The same accounting as at a read and an epoll: we left by the ceiling — and was there
        // already a ready one in the set? (poll(2) is the fourth place where the network is waited for.)
        if wait_frame_w(seq, left_ns(t0, limit), &mut _w, only_sock) {
            let ready_now = unsafe {
                let _s2 = net_lock();
                (0..nfds).any(|k| {
                    let p = fds.add(k * 8);
                    let fd = core::ptr::read_unaligned(p as *const i32) as i64;
                    let ev = core::ptr::read_unaligned(p.add(4) as *const u16) as u32;
                    fd >= 0 && any_readiness(fd) & (ev | EPOLLERR | EPOLLHUP) != 0
                })
            };
            note_cap_exit(ready_now);
        }
    }
}

// ── UDP ───────────────────────────────────────────────────────────────────────
pub fn udp_recvfrom(fd: i64, buf: *mut u8, len: usize, src: *mut u8, srclen: *mut u32, flags: u64) -> i64 {
    // We hold the lock ONLY while we work with the stack: a blocking wait
    // with the lock clamped would stop the whole machine's network for the sake of one thread.
    let mut _g = match net_lock() { Some(g) => Some(g), None => return EAGAIN };
    let i = match udp_slot(fd) { Some(x) => x, None => return EBADF };
    if uu()[i].h.is_none() { return EAGAIN; }
    let nb = uu()[i].nonblock || flags & 0x40 != 0;
    if !nb { uu()[i].tid = crate::sched::current_id() as u8; } // I am the one that will wait (see claim_fd)
    let mut _w = NetWaiter::new();   // "I am waiting for the network" — see `wait_frame_w`
    let mut first = true;
    loop {
        let seq = wake_seq(); // the snapshot BEFORE the check: see `wake_seq`
        if !first || !us(i).can_recv() { poll(); } // do not drive the stack if the data are already in the buffer
        first = false;
        let sk = us(i);
        if sk.can_recv() {
            match sk.recv() {
                Ok((data, meta)) => {
                    let n = data.len().min(len);
                    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buf, n); }
                    write_sockaddr(src, srclen, Some(meta.endpoint));
                    uu()[i].armed = false; // we took them — the next appearance of data calls the owner again
                    crate::netprof::mark_app_in(); // §7.1
                    return n as i64;
                }
                Err(_) => return EAGAIN,
            }
        }
        if nb { return EAGAIN; }
        _g = None;
        let by_cap = wait_frame_w(seq, 0, &mut _w, true);
        _g = net_lock();
        if by_cap { note_cap_exit(udp_slot(fd).is_some_and(|j| us(j).can_recv())); }
    }
}
pub fn udp_sendto(fd: i64, buf: *const u8, len: usize, dst: *const u8) -> i64 { udp_sendto_f(fd, buf, len, dst, true) }
/// flush=false: do not drive the stack after the posting (a sendmmsg batch — one poll per batch).
pub fn udp_sendto_f(fd: i64, buf: *const u8, len: usize, dst: *const u8, flush: bool) -> i64 {
    // We hold the lock ONLY while we work with the stack: a blocking wait
    // with the lock clamped would stop the whole machine's network for the sake of one thread.
    crate::netprof::mark_app_out(); // §7.1: the boundary is the entry into the sendto, before the stack's lock
    let mut _g = match net_lock() { Some(g) => Some(g), None => return EAGAIN };
    let i = match udp_slot(fd) { Some(x) => x, None => return EBADF };
    let ep = match ip_of(dst) {
        Some((a, p)) => IpEndpoint::new(a, p),
        None => match uu()[i].remote { Some(e) => e, None => return -89 }, // EDESTADDRREQ
    };
    // Where a datagram will go is decided by the routing table: the socket could have been bound to
    // one interface while the addressee lies behind another.
    let dstip = match ep.addr { IpAddress::Ipv4(a) => a.octets() };
    let want = crate::netcfg::egress(dstip).or_else(any_up);
    if let Some(x) = want {
        if !uu()[i].bound || uu()[i].ifx as usize != x {
            let p = if uu()[i].port != 0 { uu()[i].port } else { ephemeral() };
            if !attach_udp(i, x) { return EADDRINUSE; }
            if us(i).bind(p).is_err() { return EADDRINUSE; }
            uu()[i].bound = true; uu()[i].port = p;
        }
    } else { return -101; } // ENETUNREACH
    let data = unsafe { core::slice::from_raw_parts(buf, len) };
    for _ in 0..1000 {
        if us(i).can_send() {
            if let Err(e) = us(i).send_slice(data, ep) { crate::println!("net: udp send err {:?} fd={} len={}", e, fd, len); return -11; }
            if flush { poll_forced(); } return len as i64; // the pass-sending (see write)
        }
        poll();
        _g = None; yield_window(); _g = net_lock();
    }
    EAGAIN
}

/// How many calls to the device the card's driver made (the §7.1 diagnostics).
pub fn db_writes() -> Option<u64> { if ready() { if_doorbells(0) } else { None } }

/// A reset of all the sockets/epolls before a restart of the workload (supervise): an RST to all
/// the connections, the slots are free, the stack keeps living.
pub fn reset_all() {
    let _g = match net_lock() { Some(g) => g, None => return Default::default() };
    if !ready() { return; }
    for i in 0..NS { if tt()[i].k != K::Free { abort_slot(i); } }
    for i in 0..NU {
        if uu()[i].used {
            // the kernel echo, the sink and the clock's SNTP client belong to THE KERNEL rather than to the
            // workload: they must not be torn down (the SNTP socket was, at a workload's end on a Graviton,
            // and the next pass panicked on its dangling handle in smoltcp).
            let owned_by_kernel = ifopt(uu()[i].ifx as usize)
                .is_some_and(|f| f.ke == uu()[i].h || f.ks == uu()[i].h || f.nt == uu()[i].h);
            if owned_by_kernel { continue; }
            if uu()[i].h.is_some() { us(i).close(); }
            detach_udp(i);
            uu()[i] = UDP0;
        }
    }
    unsafe { for e in 0..NE { EP_USED[e] = false; } }
    poll();
}

static TOT_RX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TOT_TX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The total bytes rx/tx over all the TCP connections (live + closed) and the number of live ones.
pub fn totals() -> (u64, u64, usize) {
    let (mut rx, mut tx, mut live) = (TOT_RX.load(core::sync::atomic::Ordering::Relaxed), TOT_TX.load(core::sync::atomic::Ordering::Relaxed), 0);
    if ready() { for i in 0..NS { let t = tt()[i]; if t.k == K::Conn { rx += t.rx; tx += t.tx; live += 1; } } }
    (rx, tx, live)
}
// -- the tables for `netstat` -------------------------------------------------
/// How many slots there are in the TCP/UDP table (to be walked from the outside).
pub const NSOCK: usize = NS;
pub const NUDP: usize = NU;

/// A row of the socket table: what the operator sees in `netstat`.
#[derive(Clone, Copy)]
pub struct SockRow {
    pub fd: i64,
    /// 1 — a listener, 2 — a connection, 3 — a half-assembled incoming one (the backlog).
    pub kind: u8,
    pub lport: u16,
    pub raddr: [u8; 4],
    pub rport: u16,
    pub state: &'static str,
    pub rx: u64,
    pub tx: u64,
    pub ifx: usize,
    /// The owner thread (255 — unknown: the socket was created by the kernel rather than by a syscall).
    pub tid: u8,
}

fn state_name(st: smoltcp::socket::tcp::State) -> &'static str {
    use smoltcp::socket::tcp::State::*;
    match st {
        Closed => "CLOSED", Listen => "LISTEN", SynSent => "SYN-SENT", SynReceived => "SYN-RECV",
        Established => "ESTAB", FinWait1 => "FIN-WAIT1", FinWait2 => "FIN-WAIT2", CloseWait => "CLOSE-WAIT",
        Closing => "CLOSING", LastAck => "LAST-ACK", TimeWait => "TIME-WAIT",
    }
}

/// A row of the TCP table by the slot's number (None — the slot is free).
pub fn sock_row(i: usize) -> Option<SockRow> {
    if !ready() || i >= NS { return None; }
    let t = tt()[i];
    let kind = match t.k { K::Listen => 1, K::Conn | K::Closing => 2, K::Backlog => 3, _ => return None };
    let (raddr, rport) = if t.h.is_some() { ep_parts(s(i).remote_endpoint()) } else { ([0; 4], 0) };
    let lport = if t.port != 0 { t.port } else if t.h.is_some() { ep_parts(s(i).local_endpoint()).1 } else { 0 };
    Some(SockRow {
        fd: TCP_BASE + i as i64, kind, lport, raddr, rport,
        state: if t.h.is_some() { state_name(s(i).state()) } else { "-" },
        rx: t.rx, tx: t.tx, ifx: t.ifx as usize, tid: t.tid,
    })
}
/// A row of the UDP table: (the fd, the local port, the interface, the owner thread).
pub fn udp_row(i: usize) -> Option<(i64, u16, usize, u8)> {
    if !ready() || i >= NU { return None; }
    let u = uu()[i];
    if !u.used || !u.bound { return None; }
    Some((UDP_BASE + i as i64, u.port, u.ifx as usize, u.tid))
}

/// The counters of the card under the interface: (rx frames, rx bytes, tx frames, tx bytes,
/// transmit drops). All of them are since the card was brought up.
pub fn if_counters(i: usize) -> (u64, u64, u64, u64, u64) {
    match ifopt(i) {
        Some(f) => { let (rn, rb) = f.dev.rx_counters(); let (tn, tb, dr) = f.dev.tx_counters(); (rn, rb, tn, tb, dr) }
        None => (0, 0, 0, 0, 0),
    }
}
/// The driver's name and the counters IN ONE snapshot: the card's ownership is taken once, and
/// a line of the report cannot turn out to be internally contradictory.
pub fn if_snapshot(i: usize) -> (&'static str, u64, u64, u64, u64, u64) {
    match ifopt(i) {
        Some(f) => f.dev.snapshot().unwrap_or(("?", 0, 0, 0, 0, 0)),
        None => ("-", 0, 0, 0, 0, 0),
    }
}
/// The driver's name under the interface.
pub fn if_driver(i: usize) -> &'static str { ifopt(i).map(|f| f.dev.driver()).unwrap_or("-") }

/// How many TCP listeners and bound UDP sockets there are right now (the `top` header).
pub fn listen_udp_count() -> (usize, usize) {
    if !ready() { return (0, 0); }
    let mut l = 0; let mut u = 0;
    for i in 0..NS { if tt()[i].k == K::Listen { l += 1; } }
    for i in 0..NU { if uu()[i].used && uu()[i].bound { u += 1; } }
    (l, u)
}

/// A summary of the live connections (the console/telemetry): the fd, the remote address, the rx/tx.
pub fn dump_conns() {
    if !ready() { return; }
    for i in 0..NS {
        let t = tt()[i];
        if t.k == K::Conn && t.h.is_some() {
            let (a, p) = ep_parts(s(i).remote_endpoint());
            crate::println!("net: fd={} {}.{}.{}.{}:{} state={:?} rx={} tx={} net{}", TCP_BASE + i as i64, a[0], a[1], a[2], a[3], p, s(i).state(), t.rx, t.tx,
                t.ifx);
        } else if t.k == K::Listen { crate::println!("net: fd={} listen :{}", TCP_BASE + i as i64, t.port); }
    }
}
