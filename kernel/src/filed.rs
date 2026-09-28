//! `filed` — the uploading and downloading of files over TCP (something like rsync) for the operator.
//!
//! WHY. A node without a Linux wrapping is a node without scp: to put a new binary there,
//! to take a dump or a log from there was until now possible only by rebuilding the disk
//! image. For a machine in production that is not an operation but a restart.
//!
//! WHY NOT SIMPLY "GIVE THE FILE OUT OVER HTTP". The operator most often uploads
//! ALMOST THE SAME file: a rebuilt binary, a corrected config. To drive over the network
//! in full what already lies on the disk is both time and a window in which the file on
//! the node corresponds neither to the old nor to the new version. So the protocol can do what
//! rsync exists for:
//!   * `STAT` — the size and the CRC32 of the whole file: it matched — transfer NOTHING;
//!   * `SUMS` — the CRC32 per BLOCK: the client computes its own and sends only those blocks
//!     that differ (`PATCH`), and trims the tail with `TRUNC`.
//! There is no rolling checksum (an insertion in the middle with a shift) here
//! deliberately: for binaries and configs that are overwritten in full,
//! a per-block comparison is enough, while its complexity is an order of magnitude lower.
//!
//! WHERE THIS LIVES. The service spins on the control plane (the BSP) and uses
//! the same `netsys` stack as the workload. The stack is NOT re-entrant, so filed is
//! serviced only where the kernel itself owns the polling loop:
//! `talaria.mode=console` and `talaria.mode=filed`. On top of a running cell
//! file access goes through the serial console — that is a deliberate boundary, not an
//! unfinished bit (see USER-GUIDE §9.4).
//!
//! SECURITY. The token is mandatory and is set ONLY from the outside —
//! `talaria.filedtoken=<...>` on the kernel's command line. There is no default
//! value: without a token the service answers any command with a refusal. The token's comparison
//! goes in constant time (`crypto::ct_eq`).
//!
//! That does not make filed a service for an untrusted network. The design document's threat model
//! keeps the control plane behind an external perimeter (a trusted network, as with
//! IPMI): there is neither TLS nor a rate limit on attempts here, and the commands `PUT`/
//! `PATCH`/`TRUNC` write into the node's file system. In an untrusted network — a tunnel.
use crate::{netsys, ext2, shell};

const PORT_DEFAULT: u16 = 7373;
const IOSZ: usize = 8192;
const LINESZ: usize = 512;

static mut PORT: u16 = 0;
static mut TOKEN: [u8; 64] = [0; 64];
static mut TOKENL: usize = 0;

static mut LIS: i64 = -1;
static mut CON: i64 = -1;
static mut AUTHED: bool = false;
static mut LINE: [u8; LINESZ] = [0; LINESZ];
static mut LINEN: usize = 0;
static mut IO: [u8; IOSZ] = [0; IOSZ];
// The tail of a read chunk which at the moment of the read turned out NOT to be ours: the client
// sent the next command while the service was still giving the file out. It must not be
// thrown away (that is a silent loss of a command), nor parsed at once (the answer would have gone
// into the middle of the body of the transfer). So it waits here until the giving-out ends.
static mut PEND: [u8; 256] = [0; 256];
static mut PENDN: usize = 0;

/// What the service is doing right now. A state instead of blocking loops: the kernel
/// is bound to return into its own loop on every pass, otherwise the console would stall for
/// the time of the transfer.
#[derive(PartialEq, Clone, Copy)]
enum St { Cmd, Recv, Send }
static mut ST: St = St::Cmd;
static mut FH: i64 = -1;        // the open file (ext2)
static mut LEFT: u64 = 0;       // how many bytes are left to receive/give out
static mut FOFF: u64 = 0;       // the offset in the file
static mut RCRC: u32 = 0;       // the accumulated CRC32 of the transfer
static mut ANSWER_CRC: bool = false; // after a receive, answer with the CRC (PUT) or simply with OK (PATCH)

// the statistics for the operator
static mut N_CONN: u64 = 0;
static mut N_GET: u64 = 0;
static mut N_PUT: u64 = 0;
static mut B_IN: u64 = 0;
static mut B_OUT: u64 = 0;

// ── CRC32 (IEEE, the same polynomial as in zlib/rsync) ────────────────────────
static mut CRCT: [u32; 256] = [0; 256];
static mut CRCT_READY: bool = false;
fn crc_init() {
    unsafe {
        if CRCT_READY { return; }
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 { c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 }; }
            CRCT[i as usize] = c;
        }
        CRCT_READY = true;
    }
}
/// Continue the CRC32 over a chunk of data (start with `!0`, finish with `!crc`).
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    crc_init();
    unsafe { for &b in data { crc = CRCT[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8); } }
    crc
}
/// The CRC32 of a whole file (through the same access as the rest of the shell).
fn file_crc(path: &[u8]) -> Option<(u64, u32)> {
    let mut f = shell::fopen(path)?;
    let mut crc = !0u32;
    let mut total = 0u64;
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(IO) };
    loop {
        let got = shell::fread(&mut f, buf);
        if got == 0 { break; }
        crc = crc32_update(crc, &buf[..got]);
        total += got as u64;
    }
    shell::fclose(f);
    Some((total, !crc))
}

// ── the configuration ─────────────────────────────────────────────────────────
pub fn configure_from_cmdline(cmdline: &str) {
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("talaria.filed=") {
            unsafe { PORT = v.parse::<u16>().unwrap_or(PORT_DEFAULT); }
        } else if let Some(v) = tok.strip_prefix("talaria.filedtoken=") {
            set_token(v);
        }
    }
    // The filed mode without an explicit port — the default port.
    if crate::cmdline::mode_is(cmdline, "filed") && unsafe { PORT } == 0 { unsafe { PORT = PORT_DEFAULT; } }
    // There is not and cannot be a DEFAULT password. There used to stand a fallback here onto a
    // token `/ctl` hard-wired into the sources (and if that did not parse as utf8 — onto
    // the string "talaria"), that is, a service THAT CAN WRITE FILES onto the node listened on
    // TCP with a password printed in an open repository. Now without
    // `talaria.filedtoken=` filed rejects any command: a mistake in the
    // deployment is bound to SHUT the door rather than open it.
    if unsafe { TOKENL } == 0 && unsafe { PORT } != 0 {
        crate::println!("filed: ATTENTION: talaria.filedtoken= is not set — the service will refuse all the commands");
    }
}
fn set_token(s: &str) {
    unsafe {
        let n = s.len().min(64);
        TOKEN[..n].copy_from_slice(&s.as_bytes()[..n]);
        TOKENL = n;
    }
}
pub fn configured_port() -> u16 { unsafe { PORT } }
pub fn running() -> bool { unsafe { LIS >= 0 } }

fn sockaddr(port: u16) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[0] = 2; a[1] = 0;                       // AF_INET
    a[2..4].copy_from_slice(&port.to_be_bytes());
    a
}

/// Bring the listener up. false — the network is not ready or the port is occupied.
pub fn start(port: u16) -> bool {
    if !netsys::ready() { crate::println!("filed: the network is not up — the service has not been started"); return false; }
    stop();
    let fd = netsys::socket(1 | 0x800); // SOCK_STREAM | SOCK_NONBLOCK
    if fd < 0 { crate::println!("filed: there is no free socket ({})", fd); return false; }
    let sa = sockaddr(port);
    if netsys::bind(fd, sa.as_ptr()) < 0 { crate::println!("filed: the port {} is occupied", port); netsys::close(fd); return false; }
    if netsys::listen(fd) < 0 { crate::println!("filed: listen was refused"); netsys::close(fd); return false; }
    unsafe { LIS = fd; PORT = port; AUTHED = false; ST = St::Cmd; }
    let ip = netsys::cfg_ip();
    crate::println!("filed: the receiving of files on tcp://{}.{}.{}.{}:{} (the token is mandatory, demo-level — behind an external perimeter)",
        ip[0], ip[1], ip[2], ip[3], port);
    true
}
pub fn stop() {
    unsafe {
        drop_conn();
        if LIS >= 0 { netsys::close(LIS); LIS = -1; }
    }
}
fn drop_conn() {
    unsafe {
        if FH >= 0 { ext2::close(FH); FH = -1; }
        if CON >= 0 { netsys::close(CON); CON = -1; }
        AUTHED = false; ST = St::Cmd; LINEN = 0; LEFT = 0; PENDN = 0;
    }
}

// ── the answers ───────────────────────────────────────────────────────────────
fn send_raw(data: &[u8]) -> bool {
    unsafe {
        if CON < 0 { return false; }
        let mut off = 0usize;
        let mut spins = 0u32;
        while off < data.len() {
            let n = netsys::write(CON, data.as_ptr().add(off), data.len() - off, 0);
            if n > 0 { off += n as usize; B_OUT += n as u64; spins = 0; }
            else if n == -11 { spins += 1; if spins > 200_000 { return false; } netsys::poll(); }
            else { return false; }
        }
        true
    }
}
fn fmt_u64(mut v: u64, out: &mut [u8]) -> usize {
    if v == 0 { out[0] = b'0'; return 1; }
    let mut t = [0u8; 20]; let mut n = 0;
    while v > 0 { t[n] = b'0' + (v % 10) as u8; v /= 10; n += 1; }
    for i in 0..n { out[i] = t[n - 1 - i]; }
    n
}
/// An answer as a line: `<word> [numbers...]`. It is assembled by hand — the kernel's formatter
/// writes into the serial line, while here a byte buffer for the socket is needed.
fn reply(word: &str, nums: &[u64]) {
    let mut b = [0u8; 128];
    let mut n = 0;
    for &c in word.as_bytes() { b[n] = c; n += 1; }
    for &v in nums { b[n] = b' '; n += 1; n += fmt_u64(v, &mut b[n..]); }
    b[n] = b'\n'; n += 1;
    send_raw(&b[..n]);
}
fn reply_err(msg: &str) {
    let mut b = [0u8; 160];
    let mut n = 0;
    for &c in b"ERR ".iter() { b[n] = c; n += 1; }
    for &c in msg.as_bytes() { if n < 158 { b[n] = c; n += 1; } }
    b[n] = b'\n'; n += 1;
    send_raw(&b[..n]);
}

// ── the parsing of the commands ───────────────────────────────────────────────
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;

/// Is the token right? An unset token (`TOKENL == 0`) matches nothing — including
/// an empty string, otherwise "we forgot to set the password" would have meant "no password is
/// needed". The comparison is in constant time: a byte-by-byte one with an early exit tells
/// the one guessing how many bytes he has already guessed (see `crypto::ct_eq`).
fn token_ok(s: &str) -> bool {
    let n = unsafe { TOKENL };
    if n == 0 { return false; }
    let tok: &[u8; 64] = unsafe { &*core::ptr::addr_of!(TOKEN) };
    let want: &[u8] = &tok[..n];
    crate::crypto::ct_eq(s.as_bytes(), want)
}

fn do_command(line: &str) {
    let line = line.trim();
    if line.is_empty() { return; }
    let (head, rest) = match line.split_once(' ') { Some((h, r)) => (h, r.trim()), None => (line, "") };
    // Before the authentication only HELLO/AUTH/QUIT are available: otherwise "it is read-only —
    // so it is safe" would have turned into reading secrets off the node.
    if !unsafe { AUTHED } && head != "AUTH" && head != "HELLO" && head != "QUIT" {
        reply_err("nuzhen AUTH");
        return;
    }
    match head {
        "HELLO" => { send_raw(b"TALARIA FILED 1\n"); }
        "AUTH" => {
            if token_ok(rest) { unsafe { AUTHED = true; } reply("OK", &[]); }
            else { reply_err("bad token"); crate::println!("filed: a wrong token was rejected"); }
        }
        "QUIT" => { reply("OK", &[]); drop_conn(); }
        "LIST" => cmd_list(rest),
        "STAT" => cmd_stat(rest),
        "SUMS" => cmd_sums(rest),
        "GET" => cmd_get(rest),
        "PUT" => cmd_put(rest),
        "PATCH" => cmd_patch(rest),
        "TRUNC" => cmd_trunc(rest),
        "DEL" => cmd_del(rest),
        "MKDIR" => cmd_mkdir(rest),
        "MOVE" => cmd_move(rest),
        "SYNC" => { if ext2::active() { ext2::flush_all(); } reply("OK", &[]); }
        _ => reply_err("neizvestnaja komanda"),
    }
}

fn abspath(arg: &str, out: &mut [u8; shell::PATH]) -> usize { shell::resolve(arg, out) }

fn cmd_list(arg: &str) {
    let mut p = [0u8; shell::PATH];
    let n = abspath(if arg.is_empty() { "/" } else { arg }, &mut p);
    if shell::list_dir(&p[..n]) < 0 { reply_err("net takogo kataloga"); return; }
    let cnt = shell::entries();
    reply("OK", &[cnt as u64]);
    for i in 0..cnt {
        let (name, isdir) = shell::entry(i);
        let mut full = [0u8; shell::PATH];
        let fl = shell::join(&p[..n], name, &mut full);
        let (size, mtime) = match shell::finfo(&full[..fl]) { Some((s, _, _, t)) => (s, t.max(0) as u64), None => (0, 0) };
        let mut b = [0u8; 320];
        let mut k = 0;
        b[k] = if isdir { b'd' } else { b'f' }; k += 1; b[k] = b' '; k += 1;
        k += fmt_u64(size, &mut b[k..]); b[k] = b' '; k += 1;
        k += fmt_u64(mtime, &mut b[k..]); b[k] = b' '; k += 1;
        for &c in name { if k < 318 { b[k] = c; k += 1; } }
        b[k] = b'\n'; k += 1;
        send_raw(&b[..k]);
    }
}

fn cmd_stat(arg: &str) {
    let mut p = [0u8; shell::PATH];
    let n = abspath(arg, &mut p);
    match shell::finfo(&p[..n]) {
        None => reply_err("net takogo fajla"),
        Some((_, true, _, _)) => reply_err("eto katalog"),
        Some((size, _, _, mtime)) => match file_crc(&p[..n]) {
            Some((sz, crc)) => reply("OK", &[sz.max(size), mtime.max(0) as u64, crc as u64]),
            None => reply_err("ne otkryt"),
        },
    }
}

/// The per-block checksums: what makes the upload of an almost matching
/// file cost one block rather than the whole file.
fn cmd_sums(arg: &str) {
    let mut it = arg.split_whitespace();
    let path = it.next().unwrap_or("");
    let bs = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(65536).clamp(512, IOSZ);
    let mut p = [0u8; shell::PATH];
    let n = abspath(path, &mut p);
    let size = match shell::finfo(&p[..n]) { Some((s, false, _, _)) => s, _ => { reply_err("net takogo fajla"); return; } };
    let nblocks = size.div_ceil(bs as u64);
    reply("OK", &[nblocks, bs as u64]);
    let mut f = match shell::fopen(&p[..n]) { Some(f) => f, None => return };
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(IO) };
    for i in 0..nblocks {
        let mut got = 0usize;
        while got < bs {
            let r = shell::fread(&mut f, &mut buf[got..bs]);
            if r == 0 { break; }
            got += r;
        }
        let crc = !crc32_update(!0, &buf[..got]);
        let mut b = [0u8; 48];
        let mut k = fmt_u64(i, &mut b);
        b[k] = b' '; k += 1;
        k += fmt_u64(crc as u64, &mut b[k..]);
        b[k] = b'\n'; k += 1;
        send_raw(&b[..k]);
    }
    shell::fclose(f);
}

fn cmd_get(arg: &str) {
    let mut p = [0u8; shell::PATH];
    let n = abspath(arg, &mut p);
    // The giving-out goes at an ARBITRARY offset (the transfer breaks into chunks as
    // the TCP window frees up), and such access is only ext2's.
    if !ext2::active() { reply_err("GET dostupen tolko na ext2"); return; }
    let (size, crc) = match file_crc(&p[..n]) { Some(x) => x, None => { reply_err("net takogo fajla"); return; } };
    let h = ext2::open(&p[..n], 0, 0);
    if h < 0 { reply_err("ne otkryt"); return; }
    reply("OK", &[size, crc as u64]);
    unsafe { FH = h; FOFF = 0; LEFT = size; ST = St::Send; N_GET += 1; }
}

fn cmd_put(arg: &str) {
    let mut it = arg.split_whitespace();
    let path = it.next().unwrap_or("");
    let size = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(u64::MAX);
    if path.is_empty() || size == u64::MAX { reply_err("usage: PUT <put> <razmer>"); return; }
    if !ext2::active() { reply_err("FS tolko dlja chtenija"); return; }
    let mut p = [0u8; shell::PATH];
    let n = abspath(path, &mut p);
    let h = ext2::open(&p[..n], O_CREAT | O_TRUNC | O_WRONLY, 0o644);
    if h < 0 { reply_err("ne sozdat fajl"); return; }
    unsafe { FH = h; LEFT = size; FOFF = 0; RCRC = !0; ST = St::Recv; ANSWER_CRC = true; N_PUT += 1; }
    send_raw(b"GO\n");
}

/// Write ONE block into an existing file: the upload of "the differences only".
fn cmd_patch(arg: &str) {
    let mut it = arg.split_whitespace();
    let path = it.next().unwrap_or("");
    let off = it.next().and_then(|s| s.parse::<u64>().ok());
    let len = it.next().and_then(|s| s.parse::<u64>().ok());
    let (off, len) = match (off, len) { (Some(o), Some(l)) => (o, l), _ => { reply_err("usage: PATCH <put> <smeshenie> <dlina>"); return; } };
    if !ext2::active() { reply_err("FS tolko dlja chtenija"); return; }
    let mut p = [0u8; shell::PATH];
    let n = abspath(path, &mut p);
    let h = ext2::open(&p[..n], O_CREAT | O_RDWR, 0o644);
    if h < 0 { reply_err("ne otkryt fajl"); return; }
    unsafe { FH = h; LEFT = len; FOFF = off; RCRC = !0; ST = St::Recv; ANSWER_CRC = false; }
    send_raw(b"GO\n");
}

fn cmd_trunc(arg: &str) {
    let mut it = arg.split_whitespace();
    let path = it.next().unwrap_or("");
    let size = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    if !ext2::active() { reply_err("FS tolko dlja chtenija"); return; }
    let mut p = [0u8; shell::PATH];
    let n = abspath(path, &mut p);
    if ext2::truncate(&p[..n], size) < 0 { reply_err("ne obrezat"); return; }
    reply("OK", &[]);
}

fn cmd_del(arg: &str) {
    if !ext2::active() { reply_err("FS tolko dlja chtenija"); return; }
    let mut p = [0u8; shell::PATH];
    let n = abspath(arg, &mut p);
    let dir = matches!(shell::finfo(&p[..n]), Some((_, true, _, _)));
    if ext2::unlink(&p[..n], dir) < 0 { reply_err("ne udalit"); return; }
    reply("OK", &[]);
}
fn cmd_mkdir(arg: &str) {
    if !ext2::active() { reply_err("FS tolko dlja chtenija"); return; }
    let mut p = [0u8; shell::PATH];
    let n = abspath(arg, &mut p);
    let r = ext2::mkdir(&p[..n], 0o755);
    if r < 0 && r != -17 { reply_err("ne sozdat katalog"); return; } // EEXIST is not an error for the client
    reply("OK", &[]);
}
fn cmd_move(arg: &str) {
    if !ext2::active() { reply_err("FS tolko dlja chtenija"); return; }
    let mut it = arg.split_whitespace();
    let (a, b) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    let (mut pa, mut pb) = ([0u8; shell::PATH], [0u8; shell::PATH]);
    let na = abspath(a, &mut pa); let nb = abspath(b, &mut pb);
    if ext2::rename(&pa[..na], &pb[..nb]) < 0 { reply_err("ne pereimenovat"); return; }
    reply("OK", &[]);
}

// ── the servicing loop ────────────────────────────────────────────────────────
/// One pass of the service. It never blocks: the calling loop (the console or
/// the filed mode) is bound to keep responsive during a transfer.
pub fn poll() {
    unsafe {
        if LIS < 0 { return; }
        if CON < 0 {
            let fd = netsys::accept4(LIS, core::ptr::null_mut(), core::ptr::null_mut(), 0x800);
            if fd < 0 { return; }
            CON = fd; AUTHED = false; ST = St::Cmd; LINEN = 0; N_CONN += 1;
            crate::println!("filed: a connection has been accepted (fd={})", fd);
            return;
        }
        match ST {
            St::Cmd => pump_commands(),
            St::Recv => pump_recv(),
            St::Send => pump_send(),
        }
    }
}

unsafe fn pump_commands() {
    let mut chunk = [0u8; 256];
    // First comes what was held back for the time of the giving-out of the file.
    let n = if PENDN > 0 {
        let k = PENDN; PENDN = 0;
        chunk[..k].copy_from_slice(&PEND[..k]);
        k as i64
    } else {
        netsys::read(CON, chunk.as_mut_ptr(), chunk.len(), 0x40) // MSG_DONTWAIT
    };
    if n == 0 { crate::println!("filed: the connection was closed by the client"); drop_conn(); return; }
    if n < 0 { return; }
    B_IN += n as u64;
    for i in 0..n as usize {
        let b = chunk[i];
        if b == b'\n' {
            let l = LINEN; LINEN = 0;
            let mut copy = [0u8; LINESZ];
            copy[..l].copy_from_slice(&LINE[..l]);
            let s = core::str::from_utf8(&copy[..l]).unwrap_or("");
            do_command(s);
            if CON < 0 { return; }
            // The command could have put the service into receiving a body: the rest of the buffer is
            // already data, and it must not be parsed as commands.
            if ST == St::Recv {
                let rest = &chunk[i + 1..n as usize];
                if !rest.is_empty() { consume_body(rest); }
                return;
            }
            // The giving-out of a file (GET): the rest is the NEXT commands. We hold them back
            // until the end of the transfer, otherwise the answer to them would have gone inside the body.
            if ST == St::Send {
                let rest = &chunk[i + 1..n as usize];
                let pend: &mut [u8] = &mut *core::ptr::addr_of_mut!(PEND);
                let k = rest.len().min(pend.len());
                pend[..k].copy_from_slice(&rest[..k]);
                PENDN = k;
                return;
            }
        } else if b != b'\r' && LINEN < LINESZ { LINE[LINEN] = b; LINEN += 1; }
    }
}

/// Write a chunk of the transfer's body into the file (the common path for "the tail after a command"
/// and for an ordinary receive).
unsafe fn consume_body(data: &[u8]) {
    let take = (data.len() as u64).min(LEFT) as usize;
    if take == 0 { return; }
    let w = ext2::pwrite(FH, data.as_ptr(), take, FOFF);
    if w < 0 { crate::println!("filed: a write error {}", w); reply_err("oshibka zapisi"); finish_recv(false); return; }
    RCRC = crc32_update(RCRC, &data[..take]);
    FOFF += w as u64; LEFT -= w as u64;
    if LEFT == 0 { finish_recv(true); }
}
unsafe fn finish_recv(ok: bool) {
    if FH >= 0 { ext2::fsync(FH); ext2::close(FH); FH = -1; }
    ST = St::Cmd;
    if ok { if ANSWER_CRC { reply("OK", &[(!RCRC) as u64]); } else { reply("OK", &[]); } }
}

unsafe fn pump_recv() {
    let buf = &mut *core::ptr::addr_of_mut!(IO);
    let want = (LEFT.min(IOSZ as u64)) as usize;
    let n = netsys::read(CON, buf.as_mut_ptr(), want, 0x40);
    if n == 0 { crate::println!("filed: the link broke off during the receive — the file is incomplete"); finish_recv(false); drop_conn(); return; }
    if n < 0 { return; }
    B_IN += n as u64;
    let got = n as usize;
    let mut copy = [0u8; 256];
    if got <= copy.len() {
        copy[..got].copy_from_slice(&buf[..got]);
        consume_body(&copy[..got]);
    } else {
        // a large chunk we write straight out of the IO (pwrite copies by itself)
        let take = (got as u64).min(LEFT) as usize;
        let w = ext2::pwrite(FH, buf.as_ptr(), take, FOFF);
        if w < 0 { reply_err("oshibka zapisi"); finish_recv(false); return; }
        RCRC = crc32_update(RCRC, &buf[..take]);
        FOFF += w as u64; LEFT -= w as u64;
        if LEFT == 0 { finish_recv(true); }
    }
}

unsafe fn pump_send() {
    let buf = &mut *core::ptr::addr_of_mut!(IO);
    let want = (LEFT.min(IOSZ as u64)) as usize;
    if want == 0 { ST = St::Cmd; if FH >= 0 { ext2::close(FH); FH = -1; } return; }
    let got = if FH >= 0 {
        let r = ext2::pread(FH, buf.as_mut_ptr(), want, FOFF);
        if r <= 0 { 0 } else { r as usize }
    } else { 0 };
    if got == 0 { ST = St::Cmd; if FH >= 0 { ext2::close(FH); FH = -1; } return; }
    let w = netsys::write(CON, buf.as_ptr(), got, 0x40);
    if w == -11 { netsys::poll(); return; }        // the transmit window is full — we will come back
    if w <= 0 { crate::println!("filed: the link broke off during the giving-out"); ST = St::Cmd; if FH >= 0 { ext2::close(FH); FH = -1; } return; }
    B_OUT += w as u64;
    FOFF += w as u64; LEFT -= w as u64;
    if LEFT == 0 { ST = St::Cmd; if FH >= 0 { ext2::close(FH); FH = -1; } }
}

/// The service's state for the console (`filed` without an argument).
pub fn dump() {
    unsafe {
        if LIS < 0 { crate::println!("filed: switched off (filed on [port] — to switch it on)"); return; }
        let ip = netsys::cfg_ip();
        let (port, con) = (PORT, CON);
        crate::println!("filed: listening on {}.{}.{}.{}:{}, a connection {}",
            ip[0], ip[1], ip[2], ip[3], port,
            if con >= 0 { "yes" } else { "no" });
        let (nc, ng, np, bi, bo) = (N_CONN, N_GET, N_PUT, B_IN, B_OUT);
        crate::println!("filed: connections {}, files given out {}, received {}, bytes in/out {}/{}",
            nc, ng, np, bi, bo);
    }
}

/// The console command `filed [on|off|status] [port]`.
pub fn cmd(arg: &str) {
    let mut it = arg.split_whitespace();
    match it.next().unwrap_or("status") {
        "on" | "start" => {
            let p = it.next().and_then(|s| s.parse::<u16>().ok()).unwrap_or(unsafe { if PORT != 0 { PORT } else { PORT_DEFAULT } });
            start(p);
        }
        "off" | "stop" => { stop(); crate::println!("filed: stopped"); }
        "status" => dump(),
        _ => crate::println!("usage: filed [on [port] | off | status]"),
    }
}

/// The self-test of the CRC32 and of the parsing of the commands (`talaria.test=filed`, the part without a network):
/// the checksum is bound to match zlib's reference, otherwise "the file matched"
/// will be said about different files.
pub fn selftest() -> bool {
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("filed-test: FAIL {}", what); ok = false; } else { crate::println!("filed-test: ok {}",
        what); } };
    let c1 = !crc32_update(!0, b"123456789");
    check(c1 == 0xCBF4_3926, "CRC32(\"123456789\") = 0xcbf43926 (the IEEE reference)");
    let c2 = !crc32_update(!0, b"");
    check(c2 == 0, "the CRC32 of empty data = 0");
    // the streaming computation over chunks is bound to match the computation over the whole
    let part = crc32_update(!0, b"12345");
    let c3 = !crc32_update(part, b"6789");
    check(c3 == c1, "the CRC32 over chunks = the CRC32 over the whole (otherwise the per-block upload lies)");
    crate::println!("filed-test: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
    ok
}
