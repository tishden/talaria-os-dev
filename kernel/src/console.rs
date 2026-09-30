//! The Talaria operator console over serial — a control interface (not a Unix shell).
//!
//! In the partitioned model it spins on the BSP (the control plane) while the data
//! cell works on its own cores. Busy-poll RX, IF=0.
//!
//! WHAT IS HERE BESIDES THE COMMAND DISPATCHER. The operator console is what a
//! person uses on the worst day: the workload has crashed, the network is partial, there is no time.
//! So:
//!   * A LINE EDITOR — the arrows, the history, Ctrl-A/E/U/K/W, Tab completion of
//!     commands AND paths. Without it a single typo in a long path costs
//!     retyping the line;
//!   * HELP BY GROUP (`help`) and per command (`help <name>`) with an example —
//!     a list of 30 words on one line is not help but a reminder;
//!   * A HINT on a typo ("unknown command 'stauts' — did you mean
//!     'status'?»);
//!   * A SINGLE REFUSAL FORMAT: a command that cannot run says
//!     WHY (no FS / read-only / an interactive console is needed) rather than
//!     printing nothing.
//!
//! TWO CONTEXTS. `run()` is the interactive console (a blocking loop on the BSP,
//! everything is available, including `less` and `edit`). `poll_tick()` is the same console ON TOP
//! of a running workload, from the scheduler tick, i.e. from THE BODY OF AN INTERRUPT
//! HANDLER: blocking is forbidden there, and commands that wait for a key honestly
//! refuse instead of hanging the kernel.
use crate::{serial, mem, tsc, shell};

fn put(s: &[u8]) { for &b in s { serial::putb(b); } }

// ── the command table ─────────────────────────────────────────────────────────
const G_SYS: u8 = 0;
const G_FILE: u8 = 1;
const G_NET: u8 = 2;
const G_DIAG: u8 = 3;
const GROUPS: [&str; 4] = ["system", "files", "network", "diagnostics"];

struct Cmd {
    name: &'static str,
    args: &'static str,
    help: &'static str,
    grp: u8,
}
static CMDS: &[Cmd] = &[
    // system
    Cmd { name: "help",    args: "[command]", help: "help: without an argument by group, with one for a command", grp: G_SYS },
    Cmd { name: "status",  args: "",          help: "a summary: uptime, memory, the TSC frequency, the FS, the network", grp: G_SYS },
    Cmd { name: "uptime",  args: "",          help: "the time since boot", grp: G_SYS },
    Cmd { name: "clock",   args: "",          help: "the clock: the reference, the correction, the residual error, the backward steps", grp: G_SYS },
    Cmd { name: "mem",     args: "",          help: "the frame allocator: free/reclaim", grp: G_SYS },
    Cmd { name: "ps",      args: "[-t] [-m]",  help: "processes and threads: the core, CPU%, RSS, allocations, syscalls (-t by thread, -m without walking the \
        tables)", grp: G_SYS },
    Cmd { name: "top",     args: "[-t] [-m] [-1] [ms]", help: "the same as a refreshing table (q — quit, t — threads/processes, -1 — one snapshot)",
        grp: G_SYS },
    Cmd { name: "cores",   args: "",          help: "the placement of threads on cores and the L3 shares (CAT)", grp: G_SYS },
    Cmd { name: "restart", args: "",          help: "restart the workload (through the supervisor)", grp: G_SYS },
    Cmd { name: "reset",   args: "",          help: "reboot the machine (port 0xCF9)", grp: G_SYS },
    Cmd { name: "poweroff",args: "",          help: "power the machine off (ACPI S5)", grp: G_SYS },
    Cmd { name: "history", args: "",          help: "the console's command history", grp: G_SYS },
    Cmd { name: "clear",   args: "",          help: "clear the terminal's screen", grp: G_SYS },
    Cmd { name: "echo",    args: "<text>",   help: "print the text (a channel check)", grp: G_SYS },
    // files
    Cmd { name: "pwd",     args: "",              help: "the current directory", grp: G_FILE },
    Cmd { name: "cd",      args: "[path]",        help: "change the directory (without an argument, to the root)", grp: G_FILE },
    Cmd { name: "ls",      args: "[-l] [path]",   help: "a directory listing; -l — the permissions, size and time", grp: G_FILE },
    Cmd { name: "cat",     args: "<file>",        help: "print the whole file", grp: G_FILE },
    Cmd { name: "less",    args: "<file>",        help: "page through it (space/b/g/G/q)", grp: G_FILE },
    Cmd { name: "head",    args: "[-n N] <file>", help: "the first N lines (10 by default)", grp: G_FILE },
    Cmd { name: "tail",    args: "[-n N] <file>", help: "the last N lines (10 by default)", grp: G_FILE },
    Cmd { name: "hd",      args: "<file> [off] [len]", help: "a hexadecimal dump of the file", grp: G_FILE },
    Cmd { name: "stat",    args: "<path>",        help: "the size, permissions and modification time", grp: G_FILE },
    Cmd { name: "mkdir",   args: "<path>",        help: "create a directory", grp: G_FILE },
    Cmd { name: "rm",      args: "[-r] <path>",   help: "delete a file (or a tree with -r)", grp: G_FILE },
    Cmd { name: "cp",      args: "<from> <to>", help: "copy a file", grp: G_FILE },
    Cmd { name: "mv",      args: "<from> <to>", help: "rename/move", grp: G_FILE },
    Cmd { name: "touch",   args: "<file>",        help: "create an empty file", grp: G_FILE },
    Cmd { name: "write",   args: "<file> <text>", help: "write a line into a file (overwriting it)", grp: G_FILE },
    Cmd { name: "append",  args: "<file> <text>", help: "append a line to the end of a file", grp: G_FILE },
    Cmd { name: "edit",    args: "<file>",        help: "a full-screen editor (a subset of vi: i/x/dd/u//:wq)", grp: G_FILE },
    Cmd { name: "df",      args: "",              help: "the volume's usage and the journal's state", grp: G_FILE },
    Cmd { name: "sync",    args: "",              help: "flush the FS cache to disk", grp: G_FILE },
    // network
    Cmd { name: "net",   args: "", help: "the live TCP connections and the total traffic", grp: G_NET },
    Cmd { name: "nics",  args: "", help: "the driver table and the matching PCI devices", grp: G_NET },
    Cmd { name: "netstat", args: "[-i|-r|-s|-d]", help: "the whole network: the interfaces, the routes, the sockets with their owners, the datapath",
        grp: G_NET },
    Cmd { name: "filed", args: "[on [port]|off|status]", help: "the file upload/download service over TCP (the client is tools/talaria-file)", grp: G_NET },
    Cmd { name: "ip",    args: "[addr add|del <A.B.C.D/P> <if> | link up|down <if>]", help: "the interfaces and the addresses; without arguments, show them \
        all", grp: G_NET },
    Cmd { name: "route", args: "[add <net/len|default> via <gw> dev <if> | del <net/len>]", help: "the routing table; without arguments, show it", grp: G_NET },
    Cmd { name: "dhcp",  args: "<if> [on|off|renew]", help: "DHCP on an interface: enable, disable, renew the lease", grp: G_NET },
    Cmd { name: "dns",   args: "[A.B.C.D]", help: "the resolver (it regenerates /etc/resolv.conf for the workload)", grp: G_NET },
    // diagnostics
    Cmd { name: "stat",  args: "", help: "the kernel's counters (syscalls, faults, filter refusals)", grp: G_DIAG },
    Cmd { name: "telem", args: "", help: "the cell's telemetry (packets, gaps, restarts)", grp: G_DIAG },
    Cmd { name: "log",   args: "", help: "the kernel's event ring (the flight recorder)", grp: G_DIAG },
    Cmd { name: "dump",  args: "<address>", help: "a hexadecimal dump of 64 bytes of memory", grp: G_DIAG },
    Cmd { name: "core",  args: "[tid]", help: "take a core dump of a live task (the main thread by default); needs talaria.core=<path>", grp: G_DIAG },
    Cmd { name: "kstat", args: "[section]", help: "the kernel's counters by section: sched/mem/paging/fs/net/sync/irq/cache/syscalls", grp: G_DIAG },
    Cmd { name: "perf",  args: "[stat|poke N|on|off|top|record|code|gdb]", help: "the hardware counters (IPC, cache, branch predictor) and the sampling \
        profile", grp: G_DIAG },
    Cmd { name: "gdb",   args: "[on|break]", help: "the gdb stub on COM2: how to connect; `gdb on` — enable it, `gdb break` — stop the machine in the debugger \
        now", grp: G_DIAG },
];

fn help_all() {
    crate::println!("The Talaria operator console. The commands by group:");
    for g in 0..GROUPS.len() as u8 {
        crate::println!("");
        crate::println!("  [{}]", GROUPS[g as usize]);
        let mut seen: [&str; 48] = [""; 48];
        let mut ns = 0;
        for c in CMDS.iter().filter(|c| c.grp == g) {
            if seen[..ns].contains(&c.name) { continue; }
            if ns < seen.len() { seen[ns] = c.name; ns += 1; }
            crate::print!("    {:<9}", c.name);
            if !c.args.is_empty() { crate::print!("{:<24}", c.args); } else { crate::print!("{:<24}", ""); }
            crate::println!("{}", c.help);
        }
    }
    crate::println!("");
    crate::println!("  Tab — complete a command or a path, the up/down arrows — the history,");
    crate::println!("  Ctrl-A/E — the start/end of the line, Ctrl-U/K/W — erase, Ctrl-C — cancel.");
    crate::println!("  help <command> — details for one.");
}
fn help_one(name: &str) {
    let mut found = false;
    for c in CMDS.iter().filter(|c| c.name == name) {
        found = true;
        crate::println!("{} {}", c.name, c.args);
        crate::println!("    {}   [{}]", c.help, GROUPS[c.grp as usize]);
    }
    if !found { crate::println!("no such command: '{}' — help", name); }
}
/// A hint on a typo: the command with the longest common prefix (>=2 letters).
fn suggest(name: &str) {
    let (mut best, mut bl) = ("", 0usize);
    for c in CMDS {
        let mut k = 0;
        let (a, b) = (c.name.as_bytes(), name.as_bytes());
        while k < a.len() && k < b.len() && a[k] == b[k] { k += 1; }
        if k > bl { bl = k; best = c.name; }
    }
    if bl >= 2 { crate::println!("unknown command: '{}' — did you mean '{}'? (help)", name, best); }
    else { crate::println!("unknown command: '{}' — help will show the list", name); }
}

// ── the line editor ───────────────────────────────────────────────────────────
pub const LINE_MAX: usize = 256;
const HIST_N: usize = 16;

struct Ed {
    buf: [u8; LINE_MAX],
    len: usize,
    cur: usize,
    hist: [[u8; LINE_MAX]; HIST_N],
    hlen: [u16; HIST_N],
    hn: usize,          // how many lines are in the history
    hsel: usize,        // HIST_N = we are editing a fresh line
    esc: u8,            // 0 none, 1 saw an ESC, 2 saw ESC[
    par: u32,           // the numeric parameter of ESC[<n>~
    tabs: u8,           // consecutive Tabs (the second one shows the candidates)
    echo: bool,         // echo to the terminal (off in the self-test)
}
const ED0: Ed = Ed {
    buf: [0; LINE_MAX], len: 0, cur: 0,
    hist: [[0; LINE_MAX]; HIST_N], hlen: [0; HIST_N], hn: 0, hsel: HIST_N,
    esc: 0, par: 0, tabs: 0, echo: true,
};
static mut ED: Ed = ED0;
fn ed() -> &'static mut Ed { unsafe { &mut *core::ptr::addr_of_mut!(ED) } }

fn prompt_len() -> usize { 8 + shell::cwd().len() + 2 }
fn prompt() { put(b"talaria:"); put(shell::cwd()); put(b"> "); }

/// Redraw the whole line and put the cursor back. Cheaper than clever
/// partial updates and it does not diverge from the terminal's state.
fn refresh() {
    let e = ed();
    if !e.echo { return; }
    serial::putb(b'\r');
    prompt();
    put(&e.buf[..e.len]);
    put(b"\x1b[K");
    serial::putb(b'\r');
    let target = prompt_len() + e.cur;
    if target > 0 { crate::print!("\x1b[{}C", target); }
}

fn hist_push(line: &[u8]) {
    let e = ed();
    if line.is_empty() { return; }
    if e.hn > 0 && &e.hist[0][..e.hlen[0] as usize] == line { return; } // we do not record a repeat
    let mut i = HIST_N - 1;
    while i > 0 { e.hist[i] = e.hist[i - 1]; e.hlen[i] = e.hlen[i - 1]; i -= 1; }
    let l = line.len().min(LINE_MAX);
    e.hist[0][..l].copy_from_slice(&line[..l]);
    e.hlen[0] = l as u16;
    if e.hn < HIST_N { e.hn += 1; }
}
fn hist_load(sel: usize) {
    let e = ed();
    if sel >= e.hn { e.len = 0; e.cur = 0; e.hsel = HIST_N; return; }
    let l = e.hlen[sel] as usize;
    e.buf[..l].copy_from_slice(&e.hist[sel][..l]);
    e.len = l; e.cur = l; e.hsel = sel;
}

/// The boundaries of the token the cursor stands in (for Tab).
fn token_at_cursor() -> (usize, usize, bool) {
    let e = ed();
    let mut s = e.cur;
    while s > 0 && e.buf[s - 1] != b' ' { s -= 1; }
    let first = e.buf[..s].iter().all(|&c| c == b' ');
    (s, e.cur, first)
}
fn complete_cmd(pfx: &str, add: &mut [u8; 96]) -> (usize, usize) {
    let (mut cand, mut cl) = (0usize, 0usize);
    let mut common = [0u8; 96];
    let mut prev: [&str; 48] = [""; 48];
    let mut np = 0;
    for c in CMDS {
        if !c.name.starts_with(pfx) { continue; }
        if prev[..np].contains(&c.name) { continue; }
        if np < prev.len() { prev[np] = c.name; np += 1; }
        cand += 1;
        let nb = c.name.as_bytes();
        if cand == 1 { cl = nb.len().min(96); common[..cl].copy_from_slice(&nb[..cl]); }
        else { let mut k = 0; while k < cl && k < nb.len() && common[k] == nb[k] { k += 1; } cl = k; }
    }
    if cand == 0 || cl <= pfx.len() { return (0, cand); }
    let mut n = 0;
    for i in pfx.len()..cl { add[n] = common[i]; n += 1; }
    if cand == 1 && n < 95 { add[n] = b' '; n += 1; }
    (n, cand)
}
fn show_cmd_candidates(pfx: &str) {
    serial::putb(b'\n');
    let mut prev: [&str; 48] = [""; 48];
    let mut np = 0; let mut col = 0;
    for c in CMDS {
        if !c.name.starts_with(pfx) || prev[..np].contains(&c.name) { continue; }
        if np < prev.len() { prev[np] = c.name; np += 1; }
        crate::print!("{:<12}", c.name);
        col += 1; if col % 6 == 0 { serial::putb(b'\n'); }
    }
    if col % 6 != 0 { serial::putb(b'\n'); }
}
fn do_tab() {
    let e = ed();
    let (s, c, first) = token_at_cursor();
    let tok = core::str::from_utf8(&e.buf[s..c]).unwrap_or("");
    let mut add = [0u8; 96];
    let (n, cand) = if first { complete_cmd(tok, &mut add) } else { shell::complete_path(tok, &mut add) };
    if n > 0 {
        // insert the completion at the cursor's position
        let room = LINE_MAX - e.len;
        let n = n.min(room);
        e.buf.copy_within(e.cur..e.len, e.cur + n);
        e.buf[e.cur..e.cur + n].copy_from_slice(&add[..n]);
        e.len += n; e.cur += n;
        e.tabs = 0;
        refresh();
    } else if cand > 1 {
        e.tabs += 1;
        if e.tabs >= 2 {
            if first { show_cmd_candidates(tok) } else { shell::show_candidates(tok) }
            e.tabs = 0;
            refresh();
        }
    }
}

/// Feed a byte to the editor. Returns true if a whole line has been entered
/// (its contents are `line()`) and it must be executed.
pub fn feed(ch: u8) -> bool {
    let e = ed();
    // ── the terminal's control sequences (the arrows, Home/End/Delete) ──
    if e.esc == 1 {
        e.esc = if ch == b'[' || ch == b'O' { 2 } else { 0 };
        e.par = 0;
        return false;
    }
    if e.esc == 2 {
        if ch.is_ascii_digit() { e.par = e.par * 10 + (ch - b'0') as u32; return false; }
        e.esc = 0;
        match ch {
            b'A' => { let s = if e.hsel == HIST_N { 0 } else { e.hsel + 1 }; if s < e.hn { hist_load(s); refresh(); } }
            b'B' => { if e.hsel != HIST_N { if e.hsel == 0 { e.len = 0; e.cur = 0; e.hsel = HIST_N; } else { hist_load(e.hsel - 1); } refresh(); } }
            b'C' => { if e.cur < e.len { e.cur += 1; refresh(); } }
            b'D' => { if e.cur > 0 { e.cur -= 1; refresh(); } }
            b'H' => { e.cur = 0; refresh(); }
            b'F' => { e.cur = e.len; refresh(); }
            b'~' => match e.par {
                1 | 7 => { e.cur = 0; refresh(); }
                4 | 8 => { e.cur = e.len; refresh(); }
                3 if e.cur < e.len => { e.buf.copy_within(e.cur + 1..e.len, e.cur); e.len -= 1; refresh(); }
                _ => {}
            },
            _ => {}
        }
        return false;
    }
    if ch != b'\t' { e.tabs = 0; }
    match ch {
        27 => { e.esc = 1; }
        b'\r' | b'\n' => {
            if e.echo { serial::putb(b'\n'); }
            return true;
        }
        1 => { e.cur = 0; refresh(); }                                    // Ctrl-A
        2 => { if e.cur > 0 { e.cur -= 1; refresh(); } }                  // Ctrl-B
        3 => { if e.echo { put(b"^C\n"); } e.len = 0; e.cur = 0; e.hsel = HIST_N; if e.echo { prompt(); } } // Ctrl-C
        4 => { if e.cur < e.len { e.buf.copy_within(e.cur + 1..e.len, e.cur); e.len -= 1; refresh(); } } // Ctrl-D
        5 => { e.cur = e.len; refresh(); }                                // Ctrl-E
        6 => { if e.cur < e.len { e.cur += 1; refresh(); } }              // Ctrl-F
        8 | 127 => { if e.cur > 0 { e.buf.copy_within(e.cur..e.len, e.cur - 1); e.len -= 1; e.cur -= 1; refresh(); } }
        9 => do_tab(),
        11 => { e.len = e.cur; refresh(); }                               // Ctrl-K
        12 => { put(b"\x1b[H\x1b[2J"); refresh(); }                       // Ctrl-L
        14 => { if e.hsel != HIST_N { if e.hsel == 0 { e.len = 0; e.cur = 0; e.hsel = HIST_N; } else { hist_load(e.hsel - 1); } refresh(); } }
        16 => { let s = if e.hsel == HIST_N { 0 } else { e.hsel + 1 }; if s < e.hn { hist_load(s); refresh(); } }
        21 => { e.buf.copy_within(e.cur..e.len, 0); e.len -= e.cur; e.cur = 0; refresh(); } // Ctrl-U
        23 => {                                                           // Ctrl-W: a word to the left
            let mut s = e.cur;
            while s > 0 && e.buf[s - 1] == b' ' { s -= 1; }
            while s > 0 && e.buf[s - 1] != b' ' { s -= 1; }
            e.buf.copy_within(e.cur..e.len, s); e.len -= e.cur - s; e.cur = s; refresh();
        }
        c if c >= 0x20 && c != 0x7f && e.len < LINE_MAX => {
            e.buf.copy_within(e.cur..e.len, e.cur + 1);
            e.buf[e.cur] = c; e.len += 1; e.cur += 1;
            // printing at the end of the line is the ordinary case, and it needs no redraw
            if e.cur == e.len { if e.echo { serial::putb(c); } } else { refresh(); }
        }
        _ => {}
    }
    false
}
/// The entered line (after `feed` returned true).
pub fn line() -> &'static [u8] { let e = ed(); &e.buf[..e.len] }
/// Forget what was typed (after executing it).
fn line_taken() {
    let e = ed();
    let l = e.len;
    let mut tmp = [0u8; LINE_MAX];
    tmp[..l].copy_from_slice(&e.buf[..l]);
    hist_push(&tmp[..l]);
    e.len = 0; e.cur = 0; e.hsel = HIST_N;
}

fn cmd_history() {
    let e = ed();
    if e.hn == 0 { crate::println!("the history is empty"); return; }
    for i in (0..e.hn).rev() {
        crate::print!("  {:>3}  ", e.hn - i);
        put(&e.hist[i][..e.hlen[i] as usize]);
        serial::putb(b'\n');
    }
}

// ── the execution context ─────────────────────────────────────────────────────
/// Whether the execution comes from the scheduler tick (an interrupt handler): there one cannot
/// wait for a key — a command must refuse rather than hang the kernel.
///
/// THE CORE NUMBER rather than a machine-wide flag. There used to be one flag: while the BSP printed
/// the answer to `stat` from a tick (hundreds of milliseconds with IF=0 — every byte into the port is an
/// exit to the host), ALL the other cores considered themselves "in a tick", and the network stack
/// refused their syscalls with EAGAIN. A `close()` with EAGAIN is a socket the
/// workload will never close: on mongod the connections stayed in CLOSE-WAIT with
/// unread data exactly at the moment of the counter snapshot, the slots ran out, and
/// the server stopped accepting. A syscall on another core is not disturbed by the BSP's tick at all.
static IN_TICK_CPU: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
fn interactive() -> bool { !in_tick() }
/// Whether a command is being executed FROM A TICK on THIS core (the console on top of a live workload).
/// The network stack asks about that so as not to enter itself a second time on top of an
/// interrupted task: "the same core" inside a handler does not mean "the same call
/// chain".
pub fn in_tick() -> bool {
    IN_TICK_CPU.load(core::sync::atomic::Ordering::Relaxed) == crate::percpu::cpu_id() as i32
}
/// The gate `talaria.test=tickcmd`: execute a console command from the BSP's tick with
/// printing every `TICKCMD_NS` while the workload on the other cores hammers the
/// network syscalls. Exactly the path an operator's command takes on top of a workload
/// (`poll_tick` -> the "in a tick" marker -> `exec`), only without a keyboard.
static TICKCMD_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TICKCMD_NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TICKCMD_RUNS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn tickcmd_arm(period_ns: u64) { TICKCMD_NS.store(period_ns, core::sync::atomic::Ordering::SeqCst); }
#[inline]
pub fn tickcmd_tick() {
    let p = TICKCMD_NS.load(core::sync::atomic::Ordering::Relaxed);
    if p == 0 { return; }
    let now = crate::tsc::mono_ns();
    if now < TICKCMD_NEXT.load(core::sync::atomic::Ordering::Relaxed) { return; }
    TICKCMD_NEXT.store(now + p, core::sync::atomic::Ordering::Relaxed);
    let n = TICKCMD_RUNS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    exec_from_tick(b"uptime");
    // The gate must see that the commands from the tick REALLY ran next to the workload.
    if n == 200 { crate::println!("tickcmd: {} commands executed from the tick", n); }
}
/// Run `f` under the "in a tick" marker on this core (waiting for a device does not
/// yield the core). The marker's previous value is restored.
pub fn with_tick_marker<F: FnOnce()>(f: F) {
    let prev = IN_TICK_CPU.load(core::sync::atomic::Ordering::Relaxed);
    IN_TICK_CPU.store(crate::percpu::cpu_id() as i32, core::sync::atomic::Ordering::Relaxed);
    f();
    IN_TICK_CPU.store(prev, core::sync::atomic::Ordering::Relaxed);
}
/// Execute a command the way a tick executes it: with the "in a tick" marker on THIS core.
fn exec_from_tick(l: &[u8]) {
    IN_TICK_CPU.store(crate::percpu::cpu_id() as i32, core::sync::atomic::Ordering::Relaxed);
    exec(l);
    IN_TICK_CPU.store(-1, core::sync::atomic::Ordering::Relaxed);
}
fn need_interactive(what: &str) -> bool {
    if interactive() { return true; }
    crate::println!("{}: an interactive console is needed (talaria.mode=console); on top of a workload cat/head/tail are available", what);
    false
}

// ── the system/diagnostic commands ────────────────────────────────────────────
/// Parse an address: 0x.. (hex) or decimal.
fn parse_addr(s: &str) -> Option<u64> { shell::parse_num(s) }

/// A hex dump of 64 bytes at an address (if the address looks valid).
fn dump(addr: u64) {
    if !(0x1000..0x1_0000_0000).contains(&addr) {
        crate::println!("dump: the address {:#x} is out of range (0x1000..4GB)", addr);
        return;
    }
    crate::println!("dump {:#x}:", addr);
    for row in 0..4u64 {
        let base = addr + row * 16;
        serial::putb(b' ');
        for col in 0..16u64 {
            let v = unsafe { core::ptr::read_volatile((base + col) as *const u8) };
            let hex = b"0123456789abcdef";
            serial::putb(hex[(v >> 4) as usize]);
            serial::putb(hex[(v & 0xf) as usize]);
            serial::putb(b' ');
        }
        serial::putb(b'\n');
    }
}

/// `core [tid]` — take a core dump of a LIVE task. For a hung application that is
/// the only way to learn where exactly it stands without stopping the machine with a
/// debugger (entering the gdb stub freezes the whole kernel, including the datapath).
fn cmd_core(arg: &str) {
    let t = arg.trim().parse::<usize>().unwrap_or(0);
    crate::println!("core: taking a dump of task t{} (the registers come from the saved context: where it STANDS, not where it crashed)", t);
    if crate::coredump::dump_task(t) == 0 {
        crate::println!("core: the dump was not written");
    }
}

/// `gdb [on]` — the operator's entry into the debugger. Enabling the stub is A SEPARATE
/// command rather than automatic: the stub intercepts #DB/#BP, and on a machine where
/// the workload sets breakpoints itself that changes the system's behaviour.
fn cmd_gdb(arg: &str) {
    if arg.trim() == "break" {
        // Enter the debugger NOW: the stub lives on exceptions (#BP/#DB), and on a
        // running machine without a breakpoint `target remote` would wait forever.
        crate::gdb::enable();
        crate::println!("gdb: the machine is stopped in the debugger — `target remote :1234` from the host, `continue` releases it");
        crate::gdb::breakpoint();
        crate::println!("gdb: the machine was released by the debugger");
        return;
    }
    if arg.trim() == "on" {
        crate::gdb::enable();
        crate::println!("gdb: the stub is on on COM2 (0x2F8). To stop: `target remote :1234` from the host.");
        crate::println!("gdb: WARNING — entering the debugger stops THE WHOLE KERNEL, including the datapath.");
        return;
    }
    crate::obs::perf::cmd("gdb");
}

/// A reboot by whatever the platform has (the PCI reset control on x86, PSCI on arm64).
fn reset() -> ! {
    crate::println!("reset: rebooting...");
    crate::arch::cpu::reset_machine()
}

/// The "at a glance" summary: what an operator looks at first.
fn status() {
    let hz = tsc::calibrate_cached();
    let up = if hz > 0 { tsc::rdtsc() / hz } else { 0 };
    crate::println!("Talaria: uptime {} s ({} h), TSC {} MHz, tasks {}",
        up, up / 3600, hz / 1_000_000, crate::sched::task_count());
    crate::println!("memory: free {} MB (bump) + {} MB (reclaim)",
        mem::free_frames() * 4 / 1024, mem::free_list_pages() * 4 / 1024);
    match shell::backend() {
        shell::Backend::Ext2 => { let (bs, t, f, _, _) = crate::ext2::statfs();
            crate::println!("FS: ext2 RW, {} MB in total, {} MB free, the journal is {}",
                (t as usize * bs) >> 20, (f as usize * bs) >> 20, if crate::ext2::jrn_active() { "on" } else { "off" }); }
        shell::Backend::Tfs => crate::println!("FS: TFS (read-only), files {}", crate::fs::nfiles()),
        shell::Backend::None => crate::println!("FS: not mounted"),
    }
    if crate::netsys::ready() {
        let ip = crate::netsys::cfg_ip(); let gw = crate::netsys::cfg_gw();
        let (rx, tx, live) = crate::netsys::totals();
        crate::println!("network: {}.{}.{}.{} gateway {}.{}.{}.{}, connections {}, rx {} B, tx {} B",
            ip[0], ip[1], ip[2], ip[3], gw[0], gw[1], gw[2], gw[3], live, rx, tx);
    } else {
        crate::println!("network: the stack is not up");
    }
    crate::obs::perf::oneline();
}

// ── the dispatcher ────────────────────────────────────────────────────────────
fn exec(cmd: &[u8]) {
    let c = core::str::from_utf8(cmd).unwrap_or("").trim();
    let (head, rest) = match c.split_once(' ') { Some((h, r)) => (h, r.trim()), None => (c, "") };
    match head {
        "" => {}
        "help" | "?" => if rest.is_empty() { help_all() } else { help_one(rest) },
        "status" => status(),
        "uptime" => {
            let hz = tsc::calibrate_cached();
            crate::println!("{} s (TSC={} Hz)", if hz > 0 { tsc::rdtsc() / hz } else { 0 }, hz);
        }
        // The clock: not "what time is it" but HOW MUCH TO TRUST IT. The operator needs to
        // see the reference and the residual error before someone starts using these stamps
        // to reconstruct a trading day.
        "clock" => {
            let (ppm, err, adj, steps, back, feeds) = tsc::wall_stats();
            crate::println!("clock: unix={} s, the reference {}, the correction {} ppm, the error {} ns",
                tsc::wall_ns() / 1_000_000_000, tsc::source_name(), ppm, err);
            crate::println!("clock: samples {}, adjustments {}, steps by value {}, steps BACKWARDS {}{}",
                feeds, adj, steps, back,
                if back != 0 { " — time jumped backwards, stamps from that period cannot be compared" } else { "" });
        }
        "mem" => crate::println!("the allocator: bump-free={} frames ({} MB), the reclaim free list={} pages ({} MB)",
            mem::free_frames(), mem::free_frames() * 4 / 1024,
            mem::free_list_pages(), mem::free_list_pages() * 4 / 1024),
        "ps" => crate::obs::top::cmd_ps(rest),
        "top" => crate::obs::top::cmd(rest, interactive()),
        "cores" => { crate::affinity::dump(); crate::cat::dump(); }
        "restart" => { crate::println!("console: a restart of the workload was requested"); crate::supervise::request_restart(); }
        "reset" => reset(),
        "poweroff" => {
            // Like the power button: a SIGTERM to the workload, a deadline, a sync, S5. The command
            // used to hit PM1a_CNT at once — with a dirty block cache.
            crate::acpi::request_shutdown("a poweroff by the operator's command");
        }
        "history" => cmd_history(),
        "clear" => put(b"\x1b[H\x1b[2J"),
        "echo" => crate::println!("{}", rest),
        // ── files ──
        "pwd" => shell::cmd_pwd(),
        "cd" => shell::cmd_cd(rest),
        "ls" | "dir" => shell::cmd_ls(rest),
        "cat" => shell::cmd_cat(rest),
        "less" | "more" => if need_interactive("less") { shell::cmd_less(rest) },
        "head" => shell::cmd_head_tail(rest, true),
        "tail" => shell::cmd_head_tail(rest, false),
        "hd" | "hexdump" => shell::cmd_hexdump(rest),
        "mkdir" => shell::cmd_mkdir(rest),
        "rm" => shell::cmd_rm(rest),
        "cp" => shell::cmd_cp(rest),
        "mv" => shell::cmd_mv(rest),
        "touch" => shell::cmd_touch(rest),
        "write" => shell::cmd_write(rest, false),
        "append" => shell::cmd_write(rest, true),
        "edit" | "vi" => if need_interactive("edit") { crate::edit::run(rest) },
        "df" => shell::cmd_df(),
        "sync" => shell::cmd_sync(),
        // `stat` is both a file and a diagnostic command: with an argument it is a path,
        // without an argument it is the kernel's counters.
        "stat" => if rest.is_empty() { crate::metrics::dump() } else { shell::cmd_stat(rest) },
        // ── network ──
        "net" => { crate::netsys::dump_conns(); let (rx, tx, n) = crate::netsys::totals(); crate::println!("net: in total rx={} tx={} live={}", rx, tx, n); }
        // The driver table and what it sees in the machine: which driver takes which NIC
        // and whether that NIC can interrupt.
        "nics" => crate::netdev::dump(),
        "netstat" | "ss" => crate::obs::netstat::cmd(rest),
        "filed" => crate::filed::cmd(rest),
        "ip" | "ifconfig" => crate::netcfg::cmd_ip(rest),
        "route" => crate::netcfg::cmd_route(rest),
        "dhcp" => crate::netcfg::cmd_dhcp(rest),
        "dns" => crate::netcfg::cmd_dns(rest),
        // ── diagnostics ──
        "telem" => crate::telemetry::dump(),
        "log" => crate::fr::dump(),
        "dump" => match parse_addr(rest) { Some(a) => dump(a), None => crate::println!("usage: dump <address>  (e.g. dump 0x100000)") },
        "kstat" => crate::obs::kstat::cmd(rest),
        "core" => cmd_core(rest),
        "perf" => crate::obs::perf::cmd(rest),
        "gdb" => cmd_gdb(rest),
        _ => suggest(head),
    }
}


/// Whether to poll COM1 from the scheduler tick without a supervisor, and how often.
///
/// Polling is an access to a legacy port, i.e. an exit to the hypervisor. With a slice of
/// 0.5 ms that is two thousand exits per second for the sake of a key a
/// human presses once every few seconds. A step of ~8 ms is imperceptible to the fingers and
/// sixteen times cheaper; `talaria.console=nopoll` switches it off entirely — for a
/// machine where every microsecond matters and the console is not needed.
static NOPOLL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The TSC tick of the last poll. IT IS COUNTED BY TIME RATHER THAN BY ENTRIES INTO THE SCHEDULER.
/// The step "every Nth tick" was divided by the slice, while the counter `tk` in the BSP's
/// prologue grows on EVERY entry — and the software `int 0x41` of the network waiters too.
/// Under postgresql on core 0 there are 170 thousand such entries per second, and the port
/// was polled 2700 times per second instead of thirty: 815 thousand exits to the
/// hypervisor over a five-minute measurement, 7 µs each on average (up to 174 µs), only
/// on core 0 and exactly in the prologue, which the "tick length" histogram does not see
/// (it counts only hardware ticks). Analyses of slow deliveries showed
/// a 100-140 µs hole without a single scheduler decision — this was it.
static LAST_POLL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The step of polling COM1 from the tick. Polling is a read of a legacy port, i.e. an exit to the
/// hypervisor: on the stand it costs ~28 µs (measured by the tick's phases in `stat:`).
/// Eight milliseconds turned it into 1.8 µs on EVERY tick with a slice of
/// 0.5 ms — a third of a percent of the CPU for the keyboard. Thirty-two
/// milliseconds a human does not notice on a keypress, while the kernel does.
const POLL_NS: u64 = 32_000_000;

/// Whether it is time to poll the console on this entry into the scheduler (the argument is the entry's
/// number, kept for the signature; the decision is made by time from the TSC, see `LAST_POLL`).
#[inline]
pub fn poll_due(_tick: u64) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    if NOPOLL.load(Relaxed) { return false; }
    let now = crate::tsc::rdtsc();
    let step = crate::tsc::hz() / 1000 * (POLL_NS / 1_000_000);
    if now.wrapping_sub(LAST_POLL.load(Relaxed)) < step { return false; }
    LAST_POLL.store(now, Relaxed);
    true
}

pub fn configure_from_cmdline(cmdline: &str) {
    // The value is read AS A COMMA-SEPARATED LIST: `talaria.console=` is already taken
    // by the output channel choice (`vga`/`serial`), and `vga,nopoll` must work.
    let Some(v) = crate::cmdline::value(cmdline, "talaria.console") else { return };
    if v.split(',').any(|t| t == "nopoll") {
        // The parsing is called after EVERY cmdline merge — we print once.
        if NOPOLL.swap(true, core::sync::atomic::Ordering::SeqCst) { return; }
        crate::println!("talaria: the operator console is NOT polled from the tick (talaria.console=nopoll)");
    }
}

/// A non-blocking poll of COM1 from the scheduler tick: accumulate a line, execute it on \n.
/// It gives the operator a console (status/net/telem/restart) ON TOP of a running workload.
/// THE PRICE OF POLLING COM1 (TSC ticks: the count/sum/maximum). Reading the LSR is
/// an exit to the hypervisor, and on EC2 it cost not "microseconds" but TENS of them (see
/// `stat: console polling`); the instrument is kept so that the port can never again silently
/// eat the tail.
pub static POLL_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static POLL_CY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static POLL_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn poll_tick() {
    let t0 = crate::tsc::rdtsc();
    poll_tick_inner();
    let dt = crate::tsc::rdtsc().wrapping_sub(t0);
    POLL_N.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    POLL_CY.fetch_add(dt, core::sync::atomic::Ordering::Relaxed);
    POLL_MAX.fetch_max(dt, core::sync::atomic::Ordering::Relaxed);
}

fn poll_tick_inner() {
    // The bound is mandatory: this is THE BODY OF AN INTERRUPT HANDLER. Even if the port
    // one day starts returning bytes endlessly again, the kernel will lose a tick rather
    // than control; the rest will be read on the next tick. (Without the bound
    // the very first tick on bare metal never returned.)
    let mut budget = 64;
    while let Some(ch) = serial::getc() {
        budget -= 1;
        if budget == 0 { break; }
        if feed(ch) {
            exec_from_tick(line());
            line_taken();
            prompt();
        }
    }
}

/// A non-interactive self-test of the console (the regression `talaria.test=console`): run a
/// set of commands through exec() and make sure the dispatcher, the argument parser and
/// THE LINE EDITOR are alive. The editor is checked by feeding it bytes — exactly the
/// path an operator's keystrokes take into it.
pub fn selftest() -> ! {
    crate::println!("talaria: the operator console's self-test");
    exec(b"help");
    exec(b"help ls");
    exec(b"status");
    exec(b"mem");
    exec(b"stat");
    exec(b"telem");
    exec(b"ps");
    exec(b"top -1");     // -1: one snapshot; there is nobody to press `q` in a self-test
    exec(b"netstat");
    exec(b"kstat sched");
    exec(b"perf");
    exec(b"gdb");
    exec(b"cores");
    exec(b"nics");
    exec(b"dump 0x100000");   // hex
    exec(b"dump 1048576");    // dec (= 0x100000)
    exec(b"log");
    exec(b"pwd");
    exec(b"badcmd");
    exec(b"stauts");          // a typo -> a hint
    // ── the line editor: typing, editing, the history, completion ──
    ed().echo = false;
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("console-ed: FAIL {}", what); ok = false; } };
    // typing "status" and Backspace down to "statu"
    for &b in b"status" { feed(b); }
    check(line() == b"status", "typing a line");
    feed(8);
    check(line() == b"statu", "backspace");
    // Ctrl-A + Ctrl-D removes the first character
    feed(1); feed(4);
    check(line() == b"tatu", "Ctrl-A + Ctrl-D (the cursor to the start, delete)");
    // Ctrl-U erases everything
    feed(5); feed(21);
    check(line().is_empty(), "Ctrl-U erases the line");
    // Ctrl-W removes a word
    for &b in b"ls /etc/hosts" { feed(b); }
    feed(23);
    check(line() == b"ls ", "Ctrl-W removes a word");
    feed(21);
    // Enter puts the line into the history, the up arrow fetches it
    for &b in b"uptime" { feed(b); }
    check(feed(b'\r'), "Enter finishes the line");
    line_taken();
    check(line().is_empty(), "after Enter the line is empty");
    feed(27); feed(b'['); feed(b'A');
    check(line() == b"uptime", "the up arrow fetches the history");
    feed(27); feed(b'['); feed(b'B');
    check(line().is_empty(), "the down arrow returns an empty line");
    // Tab completes a command by its prefix
    for &b in b"upt" { feed(b); }
    feed(9);
    check(line() == b"uptime ", "Tab completes a command");
    feed(21);
    // inserting in the middle of the line
    for &b in b"lsx" { feed(b); }
    feed(2); // Ctrl-B — the cursor to the left
    feed(b' ');
    check(line() == b"ls x", "inserting a character in the middle of the line");
    feed(21);
    ed().echo = true;
    crate::println!("console-ed: the editor's checks passed ({})", if ok { "all" } else { "THERE ARE FAILURES" });
    crate::println!("console: the self-test is {}", if ok { "OK" } else { "FAILED" });
    crate::suite::park();
}

pub fn run() -> ! {
    crate::println!("talaria: the operator console is ready. `help` — the commands, Tab — completion, the arrows — the history.");
    shell::ensure_mounted();
    prompt();
    loop {
        if let Some(ch) = serial::getc() {
            if feed(ch) {
                exec(line());
                line_taken();
                prompt();
            }
        }
        // The file service is served HERE: it is a state machine and does not
        // block, so the console stays responsive during a transfer.
        if crate::filed::running() { crate::filed::poll(); }
    }
}
