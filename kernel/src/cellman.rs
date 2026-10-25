//! The workload manifest for the in-kernel supervisor: a thin declarative layer over
//! supervise.rs. The format is line-based `key=value` (# starts a comment):
//!   binary=/mdpipe
//!   args=mdpipe --replay /feed.bin
//!   restart=3
//!   env=SSL_CERT_FILE=/etc/ssl/ca.crt     (several env= lines are allowed)
//! The parser is no_std/without alloc: the paths and arguments live in fixed buffers.
//!
//! ── The placement of threads on cores ──────────────────────────────────────
//! A declaration of WHAT must be rather than of HOW to do it: the manifest does not
//! name core numbers where they do not matter — a "dedicated core" is chosen by the OS
//! itself (and it gathers one process's threads onto one NUMA node):
//!   pin=main:2            the process's main thread goes to core 2
//!   pin=main:dedicated    the main thread gets a DEDICATED core, the OS chooses the number
//!   pin=rx*:dedicated     every thread named rx* gets a dedicated core of its own
//!   pin=log*:shared       these threads go into the shared pool (spreading + balancing)
//!   pin=*:0               everything else goes to core 0 (the control plane)
//!   spread=on|off         spreading the unpinned ones over free cores (on by default)
//!   balance=on|off        periodic balancing of the shared pool (on by default)
//!   numa=on|off           gathering one process's threads onto one node (on by default)
//!   node=<N>              WHICH NUMA node to place the workload on (the OS chooses by default)
//!   pool=<name>:<cores>   a named core pool (`pool=fast:2,3` / `pool=bg:4-7,9`)
//!   pin=<pattern>:<pool name>  send the threads matching the pattern into that pool
//!   irq=<core>            where to send the devices' interrupts (core 0 by default)
//!   llc=<pool|core|*>:<mask>  a share of the common L3 for these cores (Intel CAT), e.g.
//!                         `llc=fast:0xff000` — the top 8 cache ways for them only
//!   huge=<MB>|off         2 MB LARGE PAGES for this workload: from which
//!                         mapping size to take them (`huge=2` — everywhere an
//!                         aligned chunk fits). This is a property of THE WORKLOAD rather
//!                         than of the stand: for analytics with random access over
//!                         megabytes they give tens of percent (uniqExact
//!                         253 -> 197 ms), while a latency workload pays one and a half
//!                         microseconds of p50 for them — so the machine's default is
//!                         conservative, and whoever needs them asks.
//!   core=N                A LEGACY alias for `pin=main:N`
//!
//! ── MEASURED settings (what was tuned for THIS workload) ───────────────────
//! Seven quantities that past sessions tuned BY MEASUREMENT for a particular
//! workload and then froze as a compromise machine default. These are
//! properties of the workload rather than of the stand: a database needs a large block cache and a long
//! slice, a latency service a short tick and the core yielded to network waiters.
//! If not set, the machine's default is taken (or the `talaria.*` knob, if one was given).
//!   slice=<ns>            the scheduler's slice (default 500000; a sweep on
//!                         the ClickHouse suite: 8 ms -> 308 ms, 0.5 ms -> 216 ms)
//!   idleslice=<µs>        the tick period where there is nobody to preempt (default 2000;
//!                         actors pingpong p99.9 3767 -> 473 ns)
//!   netslice=<µs>         the slice on a core where someone waits for the network (default 100;
//!                         mongodb p99 528 -> 250 µs)
//!   wakepreempt=<ns>      how much a woken thread must gain to preempt a computing one (150000)
//!   fscache=<MB>          the FS block cache (default — a sixteenth of memory)
//!   readahead=<blocks>    the read-ahead window (default — full)
//!   netyield=<µs>         how often a network waiter yields the core (default 5;
//!                         PostgreSQL p50 1148 -> 67 µs, the ceiling 8.9 -> 20.1 thousand)
//!   netmix=<ns>           a waiter whose epoll/poll set is NOT only sockets parks for at most
//!                         this long instead of spinning (`talaria.netmix`; default 0 — it
//!                         spins). PostgreSQL on a c7g.xlarge (smp=2): every process of it
//!                         waits on a latch's signalfd/self-pipe next to its sockets, so the
//!                         postmaster and the background workers spun their whole timeouts
//!                         against the backends; 1 ms: the pk ceiling 9.9 -> 18.8-18.9 thousand,
//!                         select1 26.6-26.8 -> 31.5-35.4 thousand, p99 -10..-16 % (2026-10-25)
//!   stealpush=on|off      a push: a core with two ready tasks at the tick gives away
//!                         the one that has waited longer than `talaria.stealwait` to an empty
//!                         neighbour (off by default). For computing threads — yes
//!                         (ClickHouse LIKE N=4: 0.083-0.165 -> 0.082-0.085 s);
//!                         for a latency pair — NO: a separated ping-pong pair
//!                         does not reunite (actors so-simple p50 3.1 -> 9.2 µs).
//! THE UNITS HERE ARE THE SAME AS IN THE `talaria.*` KNOB OF THE SAME NAME — deliberately, so that
//! a value tuned on a stand transfers into the manifest VERBATIM. A separate,
//! "prettier" unit system in the manifest would mean a silent thousandfold error at exactly
//! the moment the value is transferred.
//! A thread's name is what it set itself with `prctl(PR_SET_NAME)`
//! (`pthread_setname_np`); the main thread also always matches `main`.
//! The order matters: the FIRST matching rule is applied.

pub const MAXARG: usize = 48;
pub const MAXENV: usize = 16;
/// The ceiling on placement rules in one manifest.
pub const MAXPIN: usize = 12;
/// The ceiling on a thread name pattern's length (Linux's `comm` has 15 significant bytes).
pub const PATLEN: usize = 24;

/// The target of a `pin=` rule: `>= 0` — a specific logical core; these two mean "the core
/// is chosen by the OS". The difference between them is the only one, and it is the main one: a dedicated
/// core is NEVER given to anyone else (run-to-completion, without a timer tick), while a shared one
/// is divided round-robin with the other unpinned threads.
pub const PIN_DEDICATED: i32 = -2;
pub const PIN_SHARED: i32 = -3;
/// No rule is set.
pub const PIN_UNSET: i32 = -1;
/// The target is a NAMED POOL of cores: `PIN_POOL_BASE - index`. A pool is the middle ground
/// between "a specific core" and "anywhere": the operator says WHICH cores are
/// given to this class of threads, while laying them out inside and balancing between
/// them is the OS's work.
pub const PIN_POOL_BASE: i32 = -100;
pub const fn pin_pool(idx: usize) -> i32 { PIN_POOL_BASE - idx as i32 }
/// The pool's index from a rule's target (None — the target is not about a pool).
pub fn pin_pool_index(target: i32) -> Option<usize> {
    if target <= PIN_POOL_BASE { Some((PIN_POOL_BASE - target) as usize) } else { None }
}

/// The ceiling on named pools in a manifest.
pub const MAXPOOLS: usize = 6;
/// The ceiling on a pool name's length.
pub const POOLNAME: usize = 16;

/// A named group of cores: `pool=<name>:<list>`, the list being numbers and ranges
/// separated by commas (`pool=fast:2,3` / `pool=bg:4-7,10`).
#[derive(Clone, Copy)]
pub struct CorePool {
    name: [u8; POOLNAME],
    name_len: usize,
    /// The pool's cores as a bitmask (the ceiling of 64 is the width of `sched_getaffinity`).
    pub mask: u64,
}

impl CorePool {
    pub const fn empty() -> Self { CorePool { name: [0; POOLNAME], name_len: 0, mask: 0 } }
    pub fn name(&self) -> &[u8] { &self.name[..self.name_len] }
}

/// The ceiling on `llc=` rules (the hardware has only a handful of L3 classes of service too).
pub const MAXLLC: usize = 4;

/// A share of the common L3 for a group of cores (Intel CAT). `who` is a pool's name, a core number or `*`;
/// the placement on cores has already been named by the operator, and the cache is hung on THE SAME group —
/// otherwise a second, independent core nomenclature would appear, and sooner or later
/// the two would silently diverge.
#[derive(Clone, Copy)]
pub struct LlcRule {
    who: [u8; POOLNAME],
    who_len: usize,
    /// The mask of L3 ways. A HARDWARE requirement: the bits are contiguous and the mask is non-zero.
    pub mask: u32,
}

impl LlcRule {
    pub const fn empty() -> Self { LlcRule { who: [0; POOLNAME], who_len: 0, mask: 0 } }
    pub fn who(&self) -> &[u8] { &self.who[..self.who_len] }
}

/// One placement rule: a thread name pattern -> where to put it.
#[derive(Clone, Copy)]
pub struct PinRule {
    pat: [u8; PATLEN],
    pat_len: usize,
    star_head: bool,   // the pattern began with '*'
    star_tail: bool,   // the pattern ended with '*'
    /// A rule about the process's MAIN thread (the keyword `main`) rather than about a name.
    pub is_main: bool,
    pub target: i32,
}

impl PinRule {
    pub const fn empty() -> Self {
        PinRule { pat: [0; PATLEN], pat_len: 0, star_head: false, star_tail: false, is_main: false, target: PIN_UNSET }
    }
    /// The rule's pattern without the asterisks (for printing the plan).
    pub fn pattern(&self) -> &[u8] { &self.pat[..self.pat_len] }
    pub fn star_head(&self) -> bool { self.star_head }
    pub fn star_tail(&self) -> bool { self.star_tail }

    /// Whether the rule fits a thread named `name`. `leader` means it is the process's main
    /// thread (then the keyword `main` matches too).
    pub fn matches(&self, name: &[u8], leader: bool) -> bool {
        if self.is_main { return leader; }
        let p = self.pattern();
        if p.is_empty() { return self.star_head || self.star_tail; } // "*" — anything
        match (self.star_head, self.star_tail) {
            (false, false) => name == p,
            (false, true) => name.len() >= p.len() && &name[..p.len()] == p,
            (true, false) => name.len() >= p.len() && &name[name.len() - p.len()..] == p,
            (true, true) => contains(name, p),
        }
    }
}

/// Whether `pat` is inside `hay` (for the pattern `*x*`; no_std, without alloc).
fn contains(hay: &[u8], pat: &[u8]) -> bool {
    if pat.len() > hay.len() { return false; }
    for i in 0..=(hay.len() - pat.len()) { if &hay[i..i + pat.len()] == pat { return true; } }
    false
}
pub struct CellManifest {
    bin: [u8; 128], bin_len: usize,       // the binary's path (+\0 for fs::open)
    argbuf: [u8; 1024], argn: usize,      // all the arguments in a row, with \0 between them
    argoff: [usize; MAXARG], argsz: [usize; MAXARG],
    pub argc: usize,
    envbuf: [u8; 1024], envn: usize,      // the env=K=V strings in a row (\0 between them)
    envoff: [usize; MAXENV], envsz: [usize; MAXENV],
    pub envc: usize,
    pub restart: u32,
    pub watchdog: u32, // seconds without incoming data -> a restart (0 = off)
    pub core: i32,   // pinning to a core (-1 = not set); A LEGACY alias for pin=main:N
    // The placement of threads on cores
    pins: [PinRule; MAXPIN],
    pub pinc: usize,
    /// The `pin=` lines that did not parse (we complain LOUDLY: a silently
    /// ignored rule is a workload that went the wrong way, with no sign of that
    /// in the log).
    pub pin_bad: u32,
    pub spread: bool,
    pub balance: bool,
    pub numa: bool,
    /// The NUMA node to place the workload on (`numa=<N>`); -1 — the OS chooses.
    pub numa_node: i32,
    // The named core pools
    pools: [CorePool; MAXPOOLS],
    pub poolc: usize,
    /// The core for device interrupts (`irq=`); -1 — not set (the control plane).
    pub irq: i32,
    /// The LARGE PAGE threshold for this workload, megabytes (-1 — not set, take
    /// the machine's default; 0 — off). See `huge=` in the header.
    pub huge_mb: i32,
    /// THIS workload's MEASURED SETTINGS; -1 — not set, take the machine's default.
    /// The units are as in the `talaria.*` knob of the same name (see the header).
    pub slice_ns: i64,
    pub idleslice_us: i64,
    pub netslice_us: i64,
    pub wakepreempt_ns: i64,
    pub fscache_mb: i64,
    pub readahead: i64,
    pub netyield_us: i64,
    pub netmix_ns: i64,
    pub stealpush: i64,
    // The shares of the common L3 (`llc=`)
    llcs: [LlcRule; MAXLLC],
    pub llcc: usize,
    pub valid: bool,
}

impl CellManifest {
    pub const fn empty() -> Self {
        CellManifest { bin: [0; 128], bin_len: 0, argbuf: [0; 1024], argn: 0,
            argoff: [0; MAXARG], argsz: [0; MAXARG], argc: 0,
            envbuf: [0; 1024], envn: 0, envoff: [0; MAXENV], envsz: [0; MAXENV], envc: 0,
            restart: 3, watchdog: 0, core: PIN_UNSET,
            pins: [PinRule::empty(); MAXPIN], pinc: 0, pin_bad: 0,
            pools: [CorePool::empty(); MAXPOOLS], poolc: 0,
            spread: true, balance: true, numa: true, numa_node: PIN_UNSET, irq: PIN_UNSET,
            huge_mb: -1,
            slice_ns: -1, idleslice_us: -1, netslice_us: -1, wakepreempt_ns: -1,
            fscache_mb: -1, readahead: -1, netyield_us: -1, netmix_ns: -1, stealpush: -1,
            llcs: [LlcRule::empty(); MAXLLC], llcc: 0,
            valid: false }
    }

    /// The i-th placement rule.
    pub fn pin(&self, i: usize) -> &PinRule { &self.pins[i.min(MAXPIN - 1)] }

    /// The first matching rule for a thread named `name` (`leader` is the process's main
    /// thread). The order of the rules in the manifest = the priority.
    pub fn match_pin(&self, name: &[u8], leader: bool) -> Option<&PinRule> {
        for i in 0..self.pinc { if self.pins[i].matches(name, leader) { return Some(&self.pins[i]); } }
        None
    }

    /// The i-th named pool.
    pub fn pool(&self, i: usize) -> &CorePool { &self.pools[i.min(MAXPOOLS - 1)] }
    /// A pool's index by name.
    pub fn pool_index(&self, name: &[u8]) -> Option<usize> {
        for i in 0..self.poolc { if self.pools[i].name() == name { return Some(i); } }
        None
    }

    /// Declare a pool: `pool=<name>:<core list>` (numbers and ranges separated by
    /// commas). Returns false if the line did not parse.
    fn push_pool(&mut self, name: &[u8], list: &[u8]) -> bool {
        if self.poolc >= MAXPOOLS || name.is_empty() || name.len() > POOLNAME { return false; }
        if self.pool_index(name).is_some() { return false; } // the name is taken
        let mut mask = 0u64;
        for part in list.split(|&c| c == b',') {
            let part = trim(part);
            if part.is_empty() { continue; }
            let (a, b) = match part.iter().position(|&c| c == b'-') {
                Some(i) => (trim(&part[..i]), trim(&part[i + 1..])),
                None => (part, part),
            };
            if a.is_empty() || b.is_empty() { return false; }
            if !a.iter().all(|c| c.is_ascii_digit()) || !b.iter().all(|c| c.is_ascii_digit()) { return false; }
            let (lo, hi) = (parse_u32(a), parse_u32(b));
            if hi < lo || hi >= 64 { return false; } // 64 is the width of the affinity mask
            for c in lo..=hi { mask |= 1u64 << c; }
        }
        if mask == 0 { return false; }
        let mut p = CorePool::empty();
        p.name[..name.len()].copy_from_slice(name);
        p.name_len = name.len();
        p.mask = mask;
        self.pools[self.poolc] = p; self.poolc += 1;
        true
    }

    /// Add the rule `pin=<pattern>:<target>`. Returns false if the line did not
    /// parse (the caller counts such lines in `pin_bad`).
    fn push_pin(&mut self, pat: &[u8], tgt: &[u8]) -> bool {
        if self.pinc >= MAXPIN { return false; }
        // A pool's name may be written as is (`pin=rx*:fast`) or with `@`
        // (`pin=rx*:@fast`) — the second form reads unambiguously where the name
        // resembles a keyword.
        let (tgt, forced_pool) = if tgt.first() == Some(&b'@') { (&tgt[1..], true) } else { (tgt, false) };
        let target = match tgt {
            b"dedicated" | b"exclusive" | b"any" if !forced_pool => PIN_DEDICATED,
            b"shared" if !forced_pool => PIN_SHARED,
            _ => {
                if !tgt.is_empty() && tgt.iter().all(|c| c.is_ascii_digit()) && !forced_pool {
                    // A core number, below the width of the affinity mask. A
                    // larger number is refused here: cast to i32 it would turn
                    // negative and read as a special target (`4294967294` was
                    // PIN_DEDICATED, `4294967196` — pool 0).
                    if tgt.len() > 2 || parse_u32(tgt) >= 64 { return false; }
                    parse_u32(tgt) as i32
                } else {
                    // Everything else is a pool's name. The pools are parsed in the FIRST pass,
                    // so the order of the lines in the manifest does not matter:
                    // a rule may refer to a pool declared below it.
                    match self.pool_index(tgt) { Some(i) => pin_pool(i), None => return false }
                }
            }
        };
        let mut r = PinRule::empty();
        r.target = target;
        if pat == b"main" { r.is_main = true; }
        else {
            let mut p = pat;
            if p.first() == Some(&b'*') { r.star_head = true; p = &p[1..]; }
            if p.last() == Some(&b'*') { r.star_tail = true; p = &p[..p.len() - 1]; }
            if p.len() > PATLEN { return false; }
            if p.is_empty() && !r.star_head && !r.star_tail { return false; }
            r.pat[..p.len()].copy_from_slice(p);
            r.pat_len = p.len();
        }
        self.pins[self.pinc] = r; self.pinc += 1;
        true
    }

    /// A rule whose pattern and target have already been separated.
    pub fn add_pin_spec_parts(&mut self, pat: &[u8], tgt: &[u8]) {
        if !self.push_pin(trim(pat), trim(tgt)) { self.pin_bad += 1; }
    }

    /// Parse a pool declaration `<name>:<core list>` (the common point for the
    /// manifest and for the command line `talaria.pool=`).
    pub fn add_pool_spec(&mut self, spec: &[u8]) {
        let spec = trim(spec);
        if spec.is_empty() { return; }
        match spec.iter().position(|&c| c == b':') {
            Some(c) => { if !self.push_pool(trim(&spec[..c]), trim(&spec[c + 1..])) { self.pin_bad += 1; } }
            None => self.pin_bad += 1,
        }
    }

    /// The i-th `llc=` rule.
    pub fn llc(&self, i: usize) -> &LlcRule { &self.llcs[i.min(MAXLLC - 1)] }

    /// `llc=<pool|core|*>:<mask>`. The mask is hexadecimal (`0xff000`) or
    /// decimal. The contiguity of the bits is NOT checked here: that is a hardware requirement,
    /// and the rule must be rejected by whoever knows the mask's length on this chip
    /// (`cat::mask_valid`) — otherwise the manifest would start lying about something that does not exist.
    pub fn add_llc_spec(&mut self, spec: &[u8]) {
        let spec = trim(spec);
        if spec.is_empty() { return; }
        let c = match spec.iter().rposition(|&c| c == b':') { Some(c) => c, None => { self.pin_bad += 1; return; } };
        let who = trim(&spec[..c]);
        let val = trim(&spec[c + 1..]);
        if who.is_empty() || who.len() > POOLNAME || val.is_empty() || self.llcc >= MAXLLC {
            self.pin_bad += 1; return;
        }
        let mask = if val.len() > 2 && val[0] == b'0' && (val[1] | 32) == b'x' {
            let mut v = 0u32;
            for &ch in &val[2..] {
                let d = match ch { b'0'..=b'9' => ch - b'0', b'a'..=b'f' => ch - b'a' + 10, b'A'..=b'F' => ch - b'A' + 10,
                    _ => { self.pin_bad += 1; return; } };
                v = v.wrapping_mul(16) + d as u32;
            }
            v
        } else {
            if !val.iter().all(|c| c.is_ascii_digit()) { self.pin_bad += 1; return; }
            parse_u32(val)
        };
        if mask == 0 { self.pin_bad += 1; return; }
        let mut r = LlcRule::empty();
        r.who[..who.len()].copy_from_slice(who);
        r.who_len = who.len();
        r.mask = mask;
        self.llcs[self.llcc] = r;
        self.llcc += 1;
    }

    /// Parse one rule line of the form `<pattern>:<target>` (the common point for the
    /// manifest and for the command line `talaria.pin=`).
    pub fn add_pin_spec(&mut self, spec: &[u8]) {
        let spec = trim(spec);
        if spec.is_empty() { return; }
        match spec.iter().rposition(|&c| c == b':') {
            Some(c) => { if !self.push_pin(trim(&spec[..c]), trim(&spec[c + 1..])) { self.pin_bad += 1; } }
            None => self.pin_bad += 1,
        }
    }

    /// The binary's path as null-terminated (for fs::open).
    pub fn binary_cstr(&self) -> &[u8] { &self.bin[..self.bin_len + 1] }
    /// The binary's path without the \0 (for printing).
    pub fn binary(&self) -> &[u8] { &self.bin[..self.bin_len] }

    /// The i-th argument as a byte slice.
    pub fn arg(&self, i: usize) -> &[u8] {
        if i >= self.argc { return &[]; }
        &self.argbuf[self.argoff[i]..self.argoff[i] + self.argsz[i]]
    }

    /// The i-th environment string "K=V".
    pub fn env(&self, i: usize) -> &[u8] {
        if i >= self.envc { return &[]; }
        &self.envbuf[self.envoff[i]..self.envoff[i] + self.envsz[i]]
    }
    fn push_env(&mut self, a: &[u8]) {
        if self.envc >= MAXENV || self.envn + a.len() + 1 > self.envbuf.len() { return; }
        let off = self.envn;
        self.envbuf[off..off + a.len()].copy_from_slice(a);
        self.envbuf[off + a.len()] = 0;
        self.envoff[self.envc] = off; self.envsz[self.envc] = a.len();
        self.envn += a.len() + 1; self.envc += 1;
    }
    fn push_arg(&mut self, a: &[u8]) {
        if self.argc >= MAXARG || self.argn + a.len() + 1 > self.argbuf.len() { return; }
        let off = self.argn;
        self.argbuf[off..off + a.len()].copy_from_slice(a);
        self.argbuf[off + a.len()] = 0; // the \0 terminator (in case of a C string)
        self.argoff[self.argc] = off; self.argsz[self.argc] = a.len();
        self.argn += a.len() + 1; self.argc += 1;
    }
}

fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0; let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\t' || s[a] == b'\r') { a += 1; }
    while b > a && (s[b-1] == b' ' || s[b-1] == b'\t' || s[b-1] == b'\r') { b -= 1; }
    &s[a..b]
}

/// `on`/`off` (`1`/`0`, `yes`/`no`); anything else means the default value.
fn onoff(s: &[u8], dflt: bool) -> bool {
    match s {
        b"on" | b"1" | b"yes" | b"true" => true,
        b"off" | b"0" | b"no" | b"false" => false,
        _ => dflt,
    }
}

fn parse_u32(s: &[u8]) -> u32 {
    let mut v = 0u32;
    for &c in s { if c.is_ascii_digit() { v = v.wrapping_mul(10) + (c - b'0') as u32; } else { break; } }
    v
}

/// The value of a measured setting: a number, or `off`/`no` as zero. Garbage is
/// zero rather than "not set": a line the operator wrote but we did not understand
/// must not be silently replaced by a default — otherwise the manifest looks applied while
/// the workload runs on someone else's numbers. Zero means something specific for each knob, and
/// it is printed into the log explicitly.
fn parse_tune(s: &[u8]) -> i64 {
    if s == b"off" || s == b"no" { return 0; }
    parse_u32(s) as i64
}

/// Parse a manifest from bytes. Unknown keys are ignored.
pub fn parse(bytes: &[u8]) -> CellManifest {
    let mut m = CellManifest::empty();
    // THE FIRST PASS is only the pool declarations. Otherwise `pin=rx*:fast` would work
    // or not depending on whether `pool=fast:…` stands above or below it in the file,
    // while the order of lines in a declaration must mean nothing except the priority
    // of the rules themselves.
    for line in bytes.split(|&c| c == b'\n') {
        let line = trim(line);
        if line.is_empty() || line[0] == b'#' { continue; }
        let eq = match line.iter().position(|&c| c == b'=') { Some(i) => i, None => continue };
        if trim(&line[..eq]) != b"pool" { continue; }
        m.add_pool_spec(trim(&line[eq + 1..]));
    }
    for line in bytes.split(|&c| c == b'\n') {
        let line = trim(line);
        if line.is_empty() || line[0] == b'#' { continue; }
        let eq = match line.iter().position(|&c| c == b'=') { Some(i) => i, None => continue };
        let key = trim(&line[..eq]);
        let val = trim(&line[eq + 1..]);
        match key {
            b"pool" => {} // parsed in the first pass
            b"binary" => {
                let n = val.len().min(m.bin.len() - 1);
                m.bin[..n].copy_from_slice(&val[..n]);
                m.bin[n] = 0; m.bin_len = n;
                if n > 0 { m.valid = true; }
            }
            b"args" => {
                for a in val.split(|&c| c == b' ') {
                    let a = trim(a);
                    if !a.is_empty() { m.push_arg(a); }
                }
            }
            b"restart" => m.restart = parse_u32(val),
            b"env" => m.push_env(val), // env=KEY=VALUE (several lines are allowed)
            b"watchdog" => m.watchdog = parse_u32(val),
            b"core" => {
                // LEGACY: `core=N` was parsed long ago but applied by
                // nobody — the field reached the scheduler and died there.
                // Now it is exactly `pin=main:N`, i.e. a working rule.
                m.core = parse_u32(val) as i32;
                m.add_pin_spec_parts(b"main", val);
            }
            b"pin" => m.add_pin_spec(val),
            // A share of the common L3 for a group of cores (Intel CAT). It is always parsed and
            // applied only where the hardware can do it: a refusal is printed.
            b"llc" => m.add_llc_spec(val),
            // Where to send the devices' interrupts. A dedicated core will not get them
            // (the kernel refuses loudly): an interrupt on the core of a latency-critical thread
            // cancels the whole point of a dedicated core.
            b"irq" => m.irq = if val == b"controlplane" || val == b"cp" { 0 } else { parse_u32(val) as i32 },
            b"spread" => m.spread = onoff(val, true),
            b"balance" => m.balance = onoff(val, true),
            // numa=on|off — whether to gather the threads and the memory onto one node.
            b"numa" => m.numa = onoff(val, true),
            // node=<N> — SAY which node exactly (the operator knows which socket
            // the NIC and the disk are attached to, while the OS does not see that through ACPI).
            // A SEPARATE key rather than `numa=<N>`: in `numa=1` the digit 1 reads both as
            // "enabled" and as "node 1", and a grammar with two readings of one
            // line will sooner or later be applied the wrong way.
            b"node" => { m.numa = true; m.numa_node = parse_u32(val) as i32; }
            // huge=<MB>|off — large pages for THIS workload (see the header).
            b"huge" => m.huge_mb = if val == b"off" || val == b"no" { 0 } else { parse_u32(val) as i32 },
            // The measured settings (see the header). `off`/`no` means zero, and zero means
            // something specific for each of them, described at the knob itself.
            b"slice" => m.slice_ns = parse_tune(val),
            b"idleslice" => m.idleslice_us = parse_tune(val),
            b"netslice" => m.netslice_us = parse_tune(val),
            b"wakepreempt" => m.wakepreempt_ns = parse_tune(val),
            b"fscache" => m.fscache_mb = parse_tune(val),
            b"readahead" => m.readahead = parse_tune(val),
            b"netyield" => m.netyield_us = parse_tune(val),
            b"netmix" => m.netmix_ns = parse_tune(val),
            b"stealpush" => m.stealpush = if val == b"on" || val == b"yes" { 1 } else { parse_tune(val) },
            _ => {}
        }
    }
    // if args are not set — argv[0] = the binary's name (the basename after the last '/')
    if m.argc == 0 && m.bin_len > 0 {
        let b = m.binary();
        let base = b.iter().rposition(|&c| c == b'/').map(|i| i + 1).unwrap_or(0);
        let name = &b[base..];
        // a copy into a temporary buffer (b is borrowed from self.bin), then push
        let mut tmp = [0u8; 128]; let n = name.len().min(128);
        tmp[..n].copy_from_slice(&name[..n]);
        m.push_arg(&tmp[..n]);
    }
    m
}

/// APPLY THE MANIFEST'S MEASURED SETTINGS TO THE MACHINE.
///
/// Called once, when the manifest has been read but the workload has not been
/// started yet. Every applied quantity is printed: a setting about which the run's
/// log says nothing is indistinguishable from an unapplied one — and it is exactly
/// on these seven numbers that all the recent sessions' wins rest.
/// THE SETTINGS NAMED ON THE COMMAND LINE ARE NOT OVERRIDDEN BY THE MANIFEST.
///
/// A measured setting belongs to the workload and therefore lives in the manifest — which is
/// right up to the moment it has to be CHECKED. An A/B on a stand
/// can change only the command line (the image with the manifest is built
/// in advance), and while the manifest won silently, an experiment on such a knob compared two
/// identical kernels — exactly the same trouble as with the user-data tail.
///
/// So: a knob named EXPLICITLY by the operator outranks the manifest, and the refusal is
/// printed. The machine's defaults are still overridden by the manifest.
pub static TUNE_FROM_CMDLINE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub const TUNE_SLICE: u32 = 1 << 0;
pub const TUNE_IDLESLICE: u32 = 1 << 1;
pub const TUNE_NETSLICE: u32 = 1 << 2;
pub const TUNE_WAKEPREEMPT: u32 = 1 << 3;
pub const TUNE_READAHEAD: u32 = 1 << 4;
pub const TUNE_NETYIELD: u32 = 1 << 5;
pub const TUNE_FSCACHE: u32 = 1 << 6;
pub const TUNE_HUGE: u32 = 1 << 7;
pub const TUNE_NETMIX: u32 = 1 << 8;
pub fn tune_locked(bit: u32) -> bool {
    let v = TUNE_FROM_CMDLINE.load(core::sync::atomic::Ordering::SeqCst) & bit != 0;
    if v { crate::println!("manifest: the setting was not applied — it is set on the command line (the operator outranks the manifest)"); }
    v
}

#[cfg(not(talaria_host))]
pub fn apply_tuning(m: &CellManifest) {
    use core::sync::atomic::Ordering::SeqCst;
    if m.huge_mb >= 0 && !tune_locked(TUNE_HUGE) { crate::paging::huge_set_mb(m.huge_mb as u64); }
    if m.slice_ns >= 0 && !tune_locked(TUNE_SLICE) {
        // Zero here = "as in the build" (the same semantics as talaria.slice).
        crate::smp::TIMER_SLICE.store(m.slice_ns as u32, SeqCst);
        crate::println!("manifest: the scheduler's slice {} ns{}", m.slice_ns,
            if m.slice_ns == 0 { " (the build's default)" } else { "" });
    }
    if m.idleslice_us >= 0 && !tune_locked(TUNE_IDLESLICE) {
        crate::smp::IDLESLICE_NS.store(m.idleslice_us as u64 * 1000, SeqCst);
        crate::println!("manifest: the tick on a core without neighbours — once every {} µs{}", m.idleslice_us,
            if m.idleslice_us == 0 { " (off)" } else { "" });
    }
    if m.netslice_us >= 0 && !tune_locked(TUNE_NETSLICE) {
        crate::smp::NETSLICE_NS.store(m.netslice_us as u64 * 1000, SeqCst);
        crate::println!("manifest: the slice on a core with a network waiter — {} µs{}", m.netslice_us,
            if m.netslice_us == 0 { " (off: the slice is the common one)" } else { "" });
    }
    if m.wakepreempt_ns >= 0 && !tune_locked(TUNE_WAKEPREEMPT) {
        crate::sync::WAKE_PREEMPT_NS.store(m.wakepreempt_ns as u64, SeqCst);
        crate::println!("manifest: a woken thread preempts a computing one after {} ns", m.wakepreempt_ns);
    }
    if m.readahead >= 0 && !tune_locked(TUNE_READAHEAD) {
        let v = crate::ext2::ra_set(m.readahead as usize);
        crate::println!("manifest: the read-ahead window is {} blocks{}", v,
            if v == 0 { " (off)" } else { "" });
    }
    if m.netyield_us >= 0 && !tune_locked(TUNE_NETYIELD) {
        crate::netsys::NETYIELD_NS.store(m.netyield_us as u64 * 1000, SeqCst);
        crate::println!("manifest: a network waiter yields the core at least once every {} µs{}", m.netyield_us,
            if m.netyield_us == 0 { " (off)" } else { "" });
    }
    if m.netmix_ns >= 0 && !tune_locked(TUNE_NETMIX) {
        crate::netsys::netmix_set(m.netmix_ns as u64);
        crate::println!("manifest: a waiter of a mixed set (sockets and a pipe/eventfd/signalfd) parks for at most {} ns{}",
            m.netmix_ns, if m.netmix_ns == 0 { " (off: it spins)" } else { "" });
    }
    if m.stealpush >= 0 {
        crate::sched::STEAL_PUSH.store(m.stealpush != 0, SeqCst);
        crate::println!("manifest: the stealing push is {}", if m.stealpush != 0 { "on: a donor at the tick gives a spare ready task to an empty neighbour" }
            else { "off" });
    }
    // The block cache comes last: it is the only one that REALLOCATES memory, and that
    // can be done only from scratch (see ext2::cache_resize). A refusal is printed there too.
    if m.fscache_mb >= 0 && !tune_locked(TUNE_FSCACHE) { crate::ext2::cache_resize(m.fscache_mb as usize); }
}

#[cfg(not(talaria_host))]
/// A SELF-TEST OF APPLYING the measured settings (the regression `talaria.test=tuning`).
///
/// A separate gate from the parsing — because parsing and applying are DIFFERENT
/// claims, and the whole history of this file consists of keys that
/// parsed perfectly and reached nowhere (`core=N` lived like that for many sessions).
/// So the quantities here are not compared with the manifest's fields: they are read
/// BACK from the very atomics the scheduler, the FS and the network stack use,
/// and every one of them certainly differs from the machine's default.
pub fn selftest_tuning() -> ! {
    use core::sync::atomic::Ordering::SeqCst;
    const M: &[u8] = b"binary=/app\nslice=300000\nidleslice=700\nnetslice=50\nwakepreempt=90000\nreadahead=16\nnetyield=0\nnetmix=250000\nstealpush=on\nfscache=48\n";
    // We mount the disk OURSELVES: without a mounted FS the riskiest of the seven
    // quantities (the block cache, the only one that REALLOCATES memory) would not be
    // checked at all, while the gate would look green.
    if !crate::ext2::active() { crate::fs::init(); }
    let mounted = crate::ext2::active();
    let was_cache = crate::ext2::cache_mb();
    let m = parse(M);
    apply_tuning(&m);
    let got: [(&str, u64, u64); 8] = [
        ("slice(ns)", crate::smp::TIMER_SLICE.load(SeqCst) as u64, 300_000),
        ("idleslice(ns)", crate::smp::IDLESLICE_NS.load(SeqCst), 700_000),
        ("netslice(ns)", crate::smp::NETSLICE_NS.load(SeqCst), 50_000),
        ("wakepreempt(ns)", crate::sync::WAKE_PREEMPT_NS.load(SeqCst), 90_000),
        ("readahead(blocks)", crate::ext2::RA_WINDOW.load(SeqCst) as u64, 16),
        ("netyield(ns)", crate::netsys::NETYIELD_NS.load(SeqCst), 0),
        ("netmix(ns)", crate::netsys::netmix_get(), 250_000),
        ("stealpush", crate::sched::STEAL_PUSH.load(SeqCst) as u64, 1),
    ];
    let mut ok = true;
    for (name, v, want) in got {
        crate::println!("tuning: {} = {} (expected {})", name, v, want);
        if v != want { ok = false; crate::println!("tuning: FAILED — {} DID NOT REACH the machine", name); }
    }
    // The block cache: a reallocation is possible only on a mounted and clean FS, and
    // a refusal is NOT a gate failure (it is legitimate and printed). A failure is
    // "we reallocated and got the wrong size".
    let now_cache = crate::ext2::cache_mb();
    crate::println!("tuning: the block cache {} -> {} MB (the FS is mounted: {})", was_cache, now_cache, mounted);
    if mounted && now_cache != 48 && now_cache != was_cache {
        ok = false;
        crate::println!("tuning: FAILED — the cache was reallocated but its size is {} MB instead of 48", now_cache);
    }
    if mounted && now_cache != 48 {
        crate::println!("tuning: fscache= was rejected (legitimately) — the cache stayed at {} MB", now_cache);
    }
    // AND THE MOST IMPORTANT THING ABOUT THE CACHE: after a reallocation the FS must keep reading.
    // A reallocation is a change of the base address, a zeroing of the tags and a return of the old
    // area to the allocator; an error here would look not like a refusal but like a read
    // from someone else's memory, i.e. exactly the silent class that is the most
    // expensive to catch later.
    if mounted {
        let mut buf = [0u8; 32];
        let h = crate::ext2::open(b"/hello.txt\0", 0, 0);
        let n = if h >= 0 { crate::ext2::read(h, buf.as_mut_ptr(), buf.len()) } else { -1 };
        crate::println!("tuning: a read through the reallocated cache: h={} bytes={}", h, n);
        if n != 6 || &buf[..6] != b"hello\n" {
            ok = false;
            crate::println!("tuning: FAILED — after the cache reallocation the FS reads the wrong thing");
        }
    }
    if ok { crate::println!("tuning: ALL CHECKS PASSED"); }
    else { crate::println!("tuning: THERE ARE FAILURES"); }
    crate::suite::park();
}

#[cfg(not(talaria_host))]
/// A self-test of the parser (the regression `talaria.test=manifest`): parse a sample and
/// print the fields; check the expected values.
pub fn selftest() -> ! {
    const SAMPLE: &[u8] =
b"# cell manifest\nbinary=/mdpipe\nargs=mdpipe --replay /feed.bin\nrestart=5\ncore=2\n";
    // Thread placement. The rules are ORDERED, the patterns have '*' on both sides,
    // and the target is a core number / dedicated / the shared pool. Plus a deliberately broken line:
    // it must be counted rather than swallowed.
    const SAMPLE2: &[u8] =
b"binary=/app\npin=main:dedicated\npin=rx*:dedicated\npin=*worker*:shared\npin=logger:3\npin=broken\n\
  spread=off\nnuma=off\nhuge=2\nslice=300000\nidleslice=700\nnetslice=50\n\
  wakepreempt=90000\nfscache=48\nreadahead=16\nnetyield=off\nstealpush=on\n";
    fn puts(s: &[u8]) { for &b in s { crate::serial::putb(b); } }
    let m = parse(SAMPLE);
    puts(b"manifest: binary=");
    puts(m.binary());
    crate::println!(" restart={} argc={} core={}", m.restart, m.argc, m.core);
    for i in 0..m.argc {
        puts(b"manifest:   argv=");
        puts(m.arg(i));
        crate::println!("");
    }
    let mut ok = m.valid
        && m.binary() == b"/mdpipe"
        && m.restart == 5
        && m.core == 2
        && m.argc == 3
        && m.arg(0) == b"mdpipe"
        && m.arg(1) == b"--replay"
        && m.arg(2) == b"/feed.bin";
    // `core=N` is no longer "parsed and forgotten": it is a rule about the main thread.
    let legacy = m.match_pin(b"anything", true);
    ok = ok && m.pinc == 1 && legacy.map(|r| r.is_main && r.target == 2) == Some(true);
    if !ok { crate::println!("manifest: the basic parsing FAILED (or core= did not become the rule pin=main:2)"); }

    let p = parse(SAMPLE2);
    crate::println!("manifest: large pages from the manifest: huge={} MB", p.huge_mb);
    crate::println!("manifest: placement rules {} (broken {}), spread={} balance={} numa={}",
        p.pinc, p.pin_bad, p.spread, p.balance, p.numa);
    let cases: [(&[u8], bool, i32); 6] = [
        (b"anything", true,  PIN_DEDICATED), // the main thread -> pin=main:dedicated
        (b"rx0",      false, PIN_DEDICATED), // rx* -> a dedicated core
        (b"rx",       false, PIN_DEDICATED), // a prefix without a tail matches too
        (b"my_worker_3", false, PIN_SHARED), // *worker* -> the shared pool
        (b"logger",   false, 3),             // an exact name -> core 3
        (b"idle",     false, PIN_UNSET),     // no rule matched
    ];
    let mut pok = p.pinc == 4 && p.pin_bad == 1 && !p.spread && p.balance && !p.numa;
    if !pok { crate::println!("manifest: FAILED — 4 rules were expected, 1 broken, spread=off balance=on numa=off"); }
    if p.huge_mb != 2 { ok = false; crate::println!("manifest: FAILED — huge= from the manifest was not parsed (expected 2, got {})", p.huge_mb); }
    // Eight measured settings: every one must arrive with ITS OWN value, and
    // `netyield=off` must become zero rather than "not set".
    let tune: [(&str, i64, i64); 8] = [
        ("slice", p.slice_ns, 300000), ("idleslice", p.idleslice_us, 700),
        ("netslice", p.netslice_us, 50), ("wakepreempt", p.wakepreempt_ns, 90000),
        ("fscache", p.fscache_mb, 48), ("readahead", p.readahead, 16),
        ("netyield", p.netyield_us, 0), ("stealpush", p.stealpush, 1),
    ];
    for (name, got, want) in tune {
        crate::println!("manifest: the setting {}={} (expected {})", name, got, want);
        if got != want { ok = false; crate::println!("manifest: FAILED — {}= from the manifest was not parsed", name); }
    }
    // And the converse: in a manifest WITHOUT these lines all eight must stay "not
    // set", otherwise the manifest would silently override the stand's knobs.
    if m.slice_ns != -1 || m.fscache_mb != -1 || m.netyield_us != -1 || m.stealpush != -1 {
        ok = false; crate::println!("manifest: FAILED — an unset setting did not stay at -1");
    }
    for (name, leader, want) in cases {
        let got = p.match_pin(name, leader).map(|r| r.target).unwrap_or(PIN_UNSET);
        puts(b"manifest:   ");
        puts(name);
        crate::println!(" (main={}) -> the target {} (expected {})", leader, got, want);
        if got != want { pok = false; }
    }
    ok = ok && pok;
    if ok { crate::println!("manifest: ALL CHECKS PASSED"); }
    else { crate::println!("manifest: THERE ARE FAILURES"); }
    crate::suite::park();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(m: &CellManifest) -> Vec<&[u8]> { (0..m.argc).map(|i| m.arg(i)).collect() }

    #[test]
    fn binary_args_env_restart() {
        let m = parse(b"# comment\nbinary=/bin/app\nargs=app  --port 80 \nenv=HOME=/\nenv=A=b=c\nrestart=5\nwatchdog=45\n");
        assert!(m.valid);
        assert_eq!(m.binary(), b"/bin/app");
        assert_eq!(m.binary_cstr(), b"/bin/app\0");
        assert_eq!(args(&m), [&b"app"[..], b"--port", b"80"]);
        assert_eq!((m.envc, m.env(0), m.env(1)), (2, &b"HOME=/"[..], &b"A=b=c"[..]));
        assert_eq!((m.restart, m.watchdog), (5, 45));
        assert_eq!(m.arg(99), b"");
    }

    #[test]
    fn defaults_when_nothing_is_said() {
        let m = parse(b"binary=/x\n");
        assert_eq!((m.restart, m.watchdog, m.core, m.irq, m.numa_node), (3, 0, PIN_UNSET, PIN_UNSET, PIN_UNSET));
        assert!(m.spread && m.balance && m.numa);
        assert_eq!((m.huge_mb, m.slice_ns, m.fscache_mb, m.stealpush), (-1, -1, -1, -1));
        assert_eq!((m.pinc, m.poolc, m.llcc, m.pin_bad), (0, 0, 0, 0));
    }

    #[test]
    fn argv0_defaults_to_the_binary_basename() {
        assert_eq!(args(&parse(b"binary=/usr/bin/redis-server\n")), [&b"redis-server"[..]]);
        assert_eq!(args(&parse(b"binary=app\n")), [&b"app"[..]]);
    }

    #[test]
    fn empty_or_missing_binary_is_invalid() {
        assert!(!parse(b"").valid);
        assert!(!parse(b"binary=\nargs=x\n").valid);
        assert!(!parse(b"args=x\nrestart=1\n").valid);
    }

    #[test]
    fn whitespace_crlf_and_unknown_keys_are_tolerated() {
        let m = parse(b"  binary = /a \r\nfrobnicate=9\r\nnot a key line\r\nrestart=\t2\r\n");
        assert_eq!((m.binary(), m.restart), (&b"/a"[..], 2));
    }

    #[test]
    fn argument_and_env_limits_do_not_overflow() {
        let mut t = String::from("binary=/a\nargs=");
        for i in 0..MAXARG + 10 { t += &format!("a{i} "); }
        t += "\n";
        for i in 0..MAXENV + 5 { t += &format!("env=K{i}=v\n"); }
        let m = parse(t.as_bytes());
        assert_eq!((m.argc, m.envc), (MAXARG, MAXENV));
        // An argument longer than the whole argument buffer is dropped, and argv
        // falls back to the basename. Pinned here as current behaviour; a loud
        // refusal is RELEASE-OSS 9.2 follow-up.
        let long = format!("binary=/a\nargs={}\n", "x".repeat(2000));
        let m = parse(long.as_bytes());
        assert_eq!((m.argc, m.arg(0)), (1, &b"a"[..]));
    }

    #[test]
    fn long_binary_path_is_truncated_not_overflowed() {
        let t = format!("binary=/{}\n", "p".repeat(300));
        let m = parse(t.as_bytes());
        assert_eq!(m.binary().len(), 127);
        assert_eq!(*m.binary_cstr().last().unwrap(), 0);
    }

    #[test]
    fn pin_patterns_match_like_globs() {
        let m = parse(b"binary=/a\npin=main:dedicated\npin=rx*:dedicated\npin=*worker*:shared\npin=*log:3\npin=exact:2\npin=*:0\n");
        assert_eq!((m.pinc, m.pin_bad), (6, 0));
        let t = |n: &[u8], lead| m.match_pin(n, lead).map(|r| r.target);
        assert_eq!(t(b"anything", true), Some(PIN_DEDICATED));   // main rule first
        assert_eq!(t(b"rx0", false), Some(PIN_DEDICATED));
        assert_eq!(t(b"my-worker-1", false), Some(PIN_SHARED));
        assert_eq!(t(b"syslog", false), Some(3));
        assert_eq!(t(b"exact", false), Some(2));
        assert_eq!(t(b"exactly", false), Some(0));                // falls to "*"
        assert_eq!(t(b"", false), Some(0));
    }

    #[test]
    fn first_matching_rule_wins() {
        let m = parse(b"binary=/a\npin=rx*:1\npin=rx0:2\n");
        assert_eq!(m.match_pin(b"rx0", false).map(|r| r.target), Some(1));
    }

    #[test]
    fn legacy_core_is_a_main_pin() {
        let m = parse(b"binary=/a\ncore=2\n");
        assert_eq!(m.core, 2);
        let r = m.match_pin(b"whatever", true).unwrap();
        assert!(r.is_main && r.target == 2);
        assert!(m.match_pin(b"whatever", false).is_none());
    }

    #[test]
    fn broken_pins_are_counted_not_swallowed() {
        let m = parse(b"binary=/a\npin=broken\npin=:3\npin=x:nosuchpool\npin=x:\npin=ok:1\n");
        assert_eq!((m.pinc, m.pin_bad), (1, 4));
    }

    #[test]
    fn core_numbers_out_of_range_are_refused_not_turned_into_special_targets() {
        // 4294967294 as i32 == -2 == PIN_DEDICATED; 4294967196 as i32 == -100 == pool 0
        let m = parse(b"binary=/a\npool=fast:1\npin=x:4294967294\npin=y:4294967196\npin=z:64\npin=w:63\n");
        assert_eq!((m.pinc, m.pin_bad), (1, 3));
        assert_eq!(m.pin(0).target, 63);
    }

    #[test]
    fn pools_are_declared_before_rules_regardless_of_order() {
        let m = parse(b"binary=/a\npin=rx*:fast\npin=bg*:@slow\npool=fast:2,3\npool=slow:4-6,9\n");
        assert_eq!((m.poolc, m.pinc, m.pin_bad), (2, 2, 0));
        assert_eq!(m.pool(0).name(), b"fast");
        assert_eq!(m.pool(0).mask, 0b1100);
        assert_eq!(m.pool(1).mask, 0b10_0111_0000);
        assert_eq!(pin_pool_index(m.match_pin(b"rx1", false).unwrap().target), Some(0));
        assert_eq!(pin_pool_index(m.match_pin(b"bg", false).unwrap().target), Some(1));
        assert_eq!(pin_pool_index(PIN_DEDICATED), None);
    }

    #[test]
    fn bad_pools_are_refused() {
        for bad in ["pool=a:", "pool=:1", "pool=a:5-3", "pool=a:64", "pool=a:x", "pool=a:1-", "pool=aaaaaaaaaaaaaaaaa:1", "pool=a"] {
            let m = parse(format!("binary=/a\n{bad}\n").as_bytes());
            assert_eq!((m.poolc, m.pin_bad), (0, 1), "{bad}");
        }
        let dup = parse(b"binary=/a\npool=a:1\npool=a:2\n");
        assert_eq!((dup.poolc, dup.pin_bad), (1, 1));
        let mut t = String::from("binary=/a\n");
        for i in 0..MAXPOOLS + 1 { t += &format!("pool=p{i}:{i}\n"); }
        let m = parse(t.as_bytes());
        assert_eq!((m.poolc, m.pin_bad), (MAXPOOLS, 1));
    }

    #[test]
    fn keyword_names_need_at_to_be_pools() {
        let m = parse(b"binary=/a\npool=shared:3\npin=a:shared\npin=b:@shared\n");
        assert_eq!(m.match_pin(b"a", false).unwrap().target, PIN_SHARED);
        assert_eq!(pin_pool_index(m.match_pin(b"b", false).unwrap().target), Some(0));
    }

    #[test]
    fn llc_rules_parse_hex_and_decimal_masks() {
        let m = parse(b"binary=/a\nllc=fast:0xff000\nllc=3:15\nllc=*:0XF\nllc=x:0\nllc=y:0xzz\nllc=noval\n");
        assert_eq!((m.llcc, m.pin_bad), (3, 3));
        assert_eq!((m.llc(0).who(), m.llc(0).mask), (&b"fast"[..], 0xff000));
        assert_eq!((m.llc(1).who(), m.llc(1).mask), (&b"3"[..], 15));
        assert_eq!(m.llc(2).mask, 0xF);
    }

    #[test]
    fn switches_and_numa() {
        let m = parse(b"binary=/a\nspread=off\nbalance=no\nnuma=0\n");
        assert!(!m.spread && !m.balance && !m.numa);
        let m = parse(b"binary=/a\nspread=maybe\nnuma=off\nnode=1\n");
        assert!(m.spread, "unknown word keeps the default");
        assert!(m.numa && m.numa_node == 1, "node= turns numa on");
    }

    #[test]
    fn irq_and_huge() {
        assert_eq!(parse(b"binary=/a\nirq=cp\n").irq, 0);
        assert_eq!(parse(b"binary=/a\nirq=5\n").irq, 5);
        assert_eq!(parse(b"binary=/a\nhuge=off\n").huge_mb, 0);
        assert_eq!(parse(b"binary=/a\nhuge=2\n").huge_mb, 2);
    }

    #[test]
    fn tuning_values_and_off() {
        let m = parse(b"binary=/a\nslice=300000\nidleslice=700\nnetslice=off\nwakepreempt=90000\nfscache=48\nreadahead=16\nnetyield=no\nstealpush=on\n");
        assert_eq!((m.slice_ns, m.idleslice_us, m.netslice_us, m.wakepreempt_ns), (300000, 700, 0, 90000));
        assert_eq!((m.fscache_mb, m.readahead, m.netyield_us, m.stealpush), (48, 16, 0, 1));
        assert_eq!(parse(b"binary=/a\nstealpush=off\n").stealpush, 0);
    }

    #[test]
    fn helpers() {
        assert_eq!(trim(b" \t a b \r"), b"a b");
        assert_eq!(trim(b"   "), b"");
        assert_eq!(parse_u32(b"12abc"), 12);
        assert_eq!(parse_u32(b""), 0);
        assert!(contains(b"abcdef", b"cde") && !contains(b"abc", b"abcd") && contains(b"x", b""));
    }
}
