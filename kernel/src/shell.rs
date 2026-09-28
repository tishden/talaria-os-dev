//! The operator's shell: the current directory, the parsing of paths and the file commands
//! (`ls/cat/less/head/tail/hd/stat/mkdir/rm/cp/mv/touch/write/df/sync`).
//!
//! WHY NOT BUSYBOX. Busybox works and starts for us (`talaria.mode=busybox`),
//! but it is a WORKLOAD in ring 3: it needs a process, an address space, a CPU core and
//! a live scheduler. The operator's console is bound to work exactly when
//! none of that is there: the workload has crashed, the supervisor is waiting, the CPU core is busy with the datapath,
//! there may be no memory for one more process. So the operator's shell lives in
//! the kernel on the BSP (the control plane) and depends on nothing but the serial line and
//! the block device. That also removes the question "and what if busybox itself is broken".
//!
//! THE BOUNDARY. There is not and will not be any pipe, any substitutions or any scripts here: this is
//! an operator's panel, not a Unix shell. Everything more complex is the workload's business.
//!
//! There are two file systems under the commands: ext2 (RW, the regular one) and TFS1/TFS2 (read-only images
//! from `mkfs.py`). The commands are the same, the difference is hidden in the `fopen/fread` below; on a
//! read-only FS the modifying commands answer with a clear refusal rather than keep silent.
use crate::{ext2, fs, serial};

pub const PATH: usize = 192;

const O_WRONLY: u64 = 1;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;
const O_DIRECTORY: u64 = 0o200000;

// ── the current directory ─────────────────────────────────────────────────────
const fn root_path() -> [u8; PATH] { let mut a = [0u8; PATH]; a[0] = b'/'; a }
static mut CWD: [u8; PATH] = root_path();
static mut CWDL: usize = 1;

/// The operator's current directory (always absolute, always without a trailing '/').
pub fn cwd() -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(CWD) as *const u8, CWDL) }
}

/// The size of the operator's terminal: the pagination of `less` and the columns of `ls` depend on it.
/// The knobs `talaria.rows=`/`talaria.cols=` — a terminal happens to be 200x50 as well as 80x24.
static mut ROWS: usize = 24;
static mut COLS: usize = 80;
pub fn rows() -> usize { unsafe { ROWS } }
pub fn cols() -> usize { unsafe { COLS } }
pub fn configure_from_cmdline(cmdline: &str) {
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("talaria.rows=") { if let Ok(n) = v.parse::<usize>() { unsafe { ROWS = n.clamp(4, 200); } } }
        else if let Some(v) = tok.strip_prefix("talaria.cols=") { if let Ok(n) = v.parse::<usize>() { unsafe { COLS = n.clamp(20, 400); } } }
    }
}

/// Assemble an ABSOLUTE normalised path from the argument (an empty argument = the cwd).
/// `out` ends with a zero; the length without the zero is returned.
pub fn resolve(arg: &str, out: &mut [u8; PATH]) -> usize {
    let a = arg.trim();
    let mut n;
    if a.starts_with('/') { out[0] = b'/'; n = 1; }
    else { let c = cwd(); n = c.len().min(PATH - 1); out[..n].copy_from_slice(&c[..n]); }
    for comp in a.split('/') {
        if comp.is_empty() || comp == "." { continue; }
        if comp == ".." {
            while n > 1 && out[n - 1] != b'/' { n -= 1; }
            if n > 1 { n -= 1; }
            continue;
        }
        if out[n - 1] != b'/' && n < PATH - 1 { out[n] = b'/'; n += 1; }
        for &b in comp.as_bytes() { if n < PATH - 1 { out[n] = b; n += 1; } }
    }
    out[n] = 0;
    n
}

// ── the unified access to a file over ext2 and TFS ────────────────────────────
/// Which FS is mounted: it determines both the set of available commands and the text of a refusal.
#[derive(PartialEq, Clone, Copy)]
pub enum Backend { Ext2, Tfs, None }
pub fn backend() -> Backend {
    if ext2::active() { Backend::Ext2 } else if fs::mounted() { Backend::Tfs } else { Backend::None }
}
/// Mount the FS if it is not mounted yet (the console comes up earlier than
/// any mode, and "there is no disk" is bound to be said once and clearly).
pub fn ensure_mounted() -> bool {
    if backend() != Backend::None { return true; }
    if fs::init() && backend() != Backend::None { return true; }
    crate::println!("files: the FS is not mounted (there is no disk or the format is unknown)");
    false
}
fn rw() -> bool { backend() == Backend::Ext2 }
fn need_rw() -> bool {
    if rw() { return true; }
    crate::println!("files: the FS is read-only (TFS) — changes are unavailable");
    false
}

pub struct File { h: i64, ext2: bool }
/// Open a file for reading (any FS). None — there is no such file.
pub fn fopen(path: &[u8]) -> Option<File> {
    if ext2::active() {
        let h = ext2::open(path, 0, 0);
        if h < 0 { None } else { Some(File { h, ext2: true }) }
    } else {
        let mut z = [0u8; PATH];
        let n = path.len().min(PATH - 1); z[..n].copy_from_slice(&path[..n]);
        let h = fs::open(z.as_ptr());
        if h < 0 { None } else { Some(File { h, ext2: false }) }
    }
}
pub fn fread(f: &mut File, buf: &mut [u8]) -> usize {
    let r = if f.ext2 { ext2::read(f.h, buf.as_mut_ptr(), buf.len()) } else { fs::read(f.h, buf.as_mut_ptr(), buf.len()) };
    if r > 0 { r as usize } else { 0 }
}
pub fn fseek(f: &mut File, off: u64) {
    if f.ext2 { ext2::lseek(f.h, off as i64, 0); } else { fs::lseek(f.h, off as i64, 0); }
}
pub fn fclose(f: File) { if f.ext2 { ext2::close(f.h); } else { fs::close(f.h); } }

/// The size of a file and whether it is a directory, by path. None — there is no such path.
pub fn finfo(path: &[u8]) -> Option<(u64, bool, u32, i64)> {
    let mut st = [0u8; 144];
    let r = if ext2::active() { ext2::stat(path, st.as_mut_ptr()) } else {
        let mut z = [0u8; PATH]; let n = path.len().min(PATH - 1); z[..n].copy_from_slice(&path[..n]);
        fs::stat_path(z.as_ptr(), st.as_mut_ptr())
    };
    if r < 0 { return None; }
    let mode = u32::from_le_bytes(st[24..28].try_into().unwrap());
    let size = i64::from_le_bytes(st[48..56].try_into().unwrap()) as u64;
    let mtime = i64::from_le_bytes(st[88..96].try_into().unwrap());
    Some((size, mode & 0xF000 == 0x4000, mode, mtime))
}

// ── printing ──────────────────────────────────────────────────────────────────
fn puts(s: &[u8]) { for &b in s { serial::putb(b); } }
/// A byte of a file onto the screen: the unprintable we show as a dot, otherwise the terminal will go astray.
fn putfile(b: u8) { if b == b'\n' || b == b'\t' || b >= 0x20 { serial::putb(b); } else { serial::putb(b'.'); } }

/// The time (unix seconds) as `YYYY-MM-DD HH:MM` — without that `ls -l` and `stat`
/// give out a number the operator can decide nothing by.
pub fn fmt_time(t: i64, out: &mut [u8; 20]) -> usize {
    if t <= 0 { out[..1].copy_from_slice(b"-"); return 1; }
    let days = t / 86400; let secs = t % 86400;
    // civil_from_days (Hinnant): the days since the epoch -> (year, month, day)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let put2 = |v: i64, out: &mut [u8; 20], n: &mut usize| { out[*n] = b'0' + (v / 10) as u8; out[*n + 1] = b'0' + (v % 10) as u8; *n += 2; };
    out[0] = b'0' + (y / 1000 % 10) as u8; out[1] = b'0' + (y / 100 % 10) as u8;
    out[2] = b'0' + (y / 10 % 10) as u8; out[3] = b'0' + (y % 10) as u8;
    let mut n = 4;
    out[n] = b'-'; n += 1; put2(m, out, &mut n);
    out[n] = b'-'; n += 1; put2(d, out, &mut n);
    out[n] = b' '; n += 1; put2(secs / 3600, out, &mut n);
    out[n] = b':'; n += 1; put2(secs % 3600 / 60, out, &mut n);
    n
}

// ── the listing of a directory ────────────────────────────────────────────────
pub const MAXENT: usize = 256;
const ENTL: usize = 64;
static mut ENAME: [[u8; ENTL]; MAXENT] = [[0; ENTL]; MAXENT];
static mut ELEN: [u8; MAXENT] = [0; MAXENT];
static mut EDIR: [bool; MAXENT] = [false; MAXENT];
static mut EN: usize = 0;
static mut DBUF: [u8; 8192] = [0; 8192];

fn ent_push(name: &[u8], dir: bool) {
    unsafe {
        if EN >= MAXENT { return; }
        let l = name.len().min(ENTL);
        ENAME[EN][..l].copy_from_slice(&name[..l]);
        ELEN[EN] = l as u8; EDIR[EN] = dir; EN += 1;
    }
}
/// Read a directory into a static table and sort it by name.
/// It returns the number of entries, or -1 if that is not a directory.
pub fn list_dir(path: &[u8]) -> i64 {
    unsafe { EN = 0; }
    if ext2::active() {
        let h = ext2::open(path, O_DIRECTORY, 0);
        if h < 0 { return -1; }
        loop {
            let buf = unsafe { &mut *core::ptr::addr_of_mut!(DBUF) };
            let n = ext2::getdents64(h, buf.as_mut_ptr(), buf.len());
            if n <= 0 { break; }
            let mut off = 0usize;
            while off + 19 < n as usize {
                let rec = u16::from_le_bytes(buf[off + 16..off + 18].try_into().unwrap()) as usize;
                if rec == 0 { break; }
                let typ = buf[off + 18];
                let nm = &buf[off + 19..off + rec.min(n as usize)];
                let nl = nm.iter().position(|&c| c == 0).unwrap_or(nm.len());
                if nl > 0 && &nm[..nl] != b"." && &nm[..nl] != b".." { ent_push(&nm[..nl], typ == 4); }
                off += rec;
            }
        }
        ext2::close(h);
    } else {
        // TFS: a flat table of paths — a directory is a common prefix, and we show only one
        // level of nesting (otherwise `ls /` would give out the whole tree).
        let pfx = if path == b"/" { &b""[..] } else { &path[1..] };
        for i in 0..fs::nfiles() {
            let (nm, nl, _sz, isdir) = match fs::entry(i) { Some(e) => e, None => continue };
            let name = &nm[..nl];
            if !pfx.is_empty() {
                if name.len() <= pfx.len() || !name.starts_with(pfx) || name[pfx.len()] != b'/' { continue; }
            }
            let rest = if pfx.is_empty() { name } else { &name[pfx.len() + 1..] };
            let cut = rest.iter().position(|&c| c == b'/');
            match cut {
                Some(k) => { let seg = &rest[..k]; if !ent_has(seg) { ent_push(seg, true); } }
                None => ent_push(rest, isdir),
            }
        }
        if unsafe { EN } == 0 && path != b"/" { return -1; }
    }
    unsafe {
        // an insertion sort: there are few entries, while the order in an ext2 directory is
        // the hash index's, that is random for a human
        for i in 1..EN {
            let mut j = i;
            while j > 0 && ent_cmp(j, j - 1) {
                (*core::ptr::addr_of_mut!(ENAME)).swap(j, j - 1);
                (*core::ptr::addr_of_mut!(ELEN)).swap(j, j - 1);
                (*core::ptr::addr_of_mut!(EDIR)).swap(j, j - 1);
                j -= 1;
            }
        }
        EN as i64
    }
}
fn ent_has(seg: &[u8]) -> bool {
    unsafe { for i in 0..EN { if &ENAME[i][..ELEN[i] as usize] == seg { return true; } } }
    false
}
fn ent_cmp(a: usize, b: usize) -> bool { // ENAME[a] < ENAME[b]
    unsafe { ENAME[a][..ELEN[a] as usize] < ENAME[b][..ELEN[b] as usize] }
}
/// The entry i of the last `list_dir` (the name, whether it is a directory).
pub fn entry(i: usize) -> (&'static [u8], bool) {
    unsafe { (core::slice::from_raw_parts(core::ptr::addr_of!(ENAME[i]) as *const u8, ELEN[i] as usize), EDIR[i]) }
}
pub fn entries() -> usize { unsafe { EN } }

// ── the commands ──────────────────────────────────────────────────────────────
pub fn cmd_pwd() { puts(cwd()); serial::putb(b'\n'); }

pub fn cmd_cd(arg: &str) {
    if !ensure_mounted() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(if arg.is_empty() { "/" } else { arg }, &mut p);
    match finfo(&p[..n]) {
        Some((_, true, _, _)) => unsafe {
            CWD[..n].copy_from_slice(&p[..n]); CWDL = n;
        },
        Some(_) => crate::println!("cd: {} — not a directory", core::str::from_utf8(&p[..n]).unwrap_or("?")),
        None => crate::println!("cd: {} — there is no such directory", core::str::from_utf8(&p[..n]).unwrap_or("?")),
    }
}

pub fn cmd_ls(arg: &str) {
    if !ensure_mounted() { return; }
    let (long, rest) = match arg.strip_prefix("-l") { Some(r) => (true, r.trim()), None => (false, arg.trim()) };
    let mut p = [0u8; PATH];
    let n = resolve(rest, &mut p);
    // A file rather than a directory: we show it alone (as ls does).
    if let Some((sz, false, mode, mt)) = finfo(&p[..n]) {
        if long { print_long(&p[..n], sz, mode, mt); } else { puts(&p[..n]); serial::putb(b'\n'); }
        return;
    }
    if list_dir(&p[..n]) < 0 { crate::println!("ls: {} — there is no such path", core::str::from_utf8(&p[..n]).unwrap_or("?")); return; }
    let cnt = entries();
    if cnt == 0 { crate::println!("(empty)"); return; }
    if long {
        for i in 0..cnt {
            let (name, _) = entry(i);
            let mut full = [0u8; PATH];
            let fl = join(&p[..n], name, &mut full);
            match finfo(&full[..fl]) {
                Some((sz, _, mode, mt)) => print_long(name, sz, mode, mt),
                None => { puts(name); serial::putb(b'\n'); }
            }
        }
        crate::println!("entries in total: {}", cnt);
    } else {
        // the columns by the terminal's width: a long list in one column is unreadable
        let mut w = 0usize;
        for i in 0..cnt { let (nm, d) = entry(i); w = w.max(nm.len() + if d { 1 } else { 0 }); }
        let w = w + 2;
        let per = (cols() / w).max(1);
        let mut col = 0;
        for i in 0..cnt {
            let (nm, d) = entry(i);
            puts(nm);
            let mut used = nm.len();
            if d { serial::putb(b'/'); used += 1; }
            col += 1;
            if col == per || i + 1 == cnt { serial::putb(b'\n'); col = 0; }
            else { for _ in used..w { serial::putb(b' '); } }
        }
    }
}
fn print_long(name: &[u8], sz: u64, mode: u32, mt: i64) {
    let t = mode & 0xF000;
    let kind = if t == 0x4000 { b'd' } else if t == 0xA000 { b'l' } else { b'-' };
    let mut perm = [b'-'; 9];
    for i in 0..9 { if mode & (1 << (8 - i)) != 0 { perm[i] = b"rwx"[i % 3]; } }
    let mut ts = [0u8; 20]; let tn = fmt_time(mt, &mut ts);
    serial::putb(kind); puts(&perm); crate::print!(" {:>10} ", sz);
    puts(&ts[..tn]); serial::putb(b' '); puts(name);
    if t == 0x4000 { serial::putb(b'/'); }
    serial::putb(b'\n');
}
/// Glue a directory and a name into an absolute path.
pub fn join(dir: &[u8], name: &[u8], out: &mut [u8; PATH]) -> usize {
    let mut n = dir.len().min(PATH - 2);
    out[..n].copy_from_slice(&dir[..n]);
    if n == 0 || out[n - 1] != b'/' { out[n] = b'/'; n += 1; }
    let l = name.len().min(PATH - 1 - n);
    out[n..n + l].copy_from_slice(&name[..l]); n += l;
    out[n] = 0;
    n
}

static mut IOBUF: [u8; 4096] = [0; 4096];
fn iobuf() -> &'static mut [u8; 4096] { unsafe { &mut *core::ptr::addr_of_mut!(IOBUF) } }

pub fn cmd_cat(arg: &str) {
    if arg.is_empty() { crate::println!("usage: cat <file>"); return; }
    if !ensure_mounted() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(arg, &mut p);
    if let Some((_, true, _, _)) = finfo(&p[..n]) { crate::println!("cat: {} — that is a directory", arg); return; }
    let mut f = match fopen(&p[..n]) { Some(f) => f, None => { crate::println!("cat: {} — there is no such file", arg); return; } };
    loop {
        let got = fread(&mut f, iobuf());
        if got == 0 { break; }
        for i in 0..got { putfile(iobuf()[i]); }
    }
    fclose(f);
}

pub fn cmd_head_tail(arg: &str, head: bool) {
    let (cnt, path) = parse_n(arg, 10);
    if path.is_empty() { crate::println!("usage: {} [-n N] <file>", if head { "head" } else { "tail" }); return; }
    if !ensure_mounted() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(path, &mut p);
    let size = match finfo(&p[..n]) { Some((s, false, _, _)) => s, Some(_) => { crate::println!("{}: that is a directory", if head { "head" } else { "tail" }); return; }, None => { crate::println!("there is no such file: {}", path); return; } };
    let mut f = match fopen(&p[..n]) { Some(f) => f, None => return };
    if head {
        let mut lines = 0usize;
        'outer: loop {
            let got = fread(&mut f, iobuf());
            if got == 0 { break; }
            for i in 0..got {
                putfile(iobuf()[i]);
                if iobuf()[i] == b'\n' { lines += 1; if lines >= cnt { break 'outer; } }
            }
        }
    } else {
        // the tail: we go backwards in windows until we have collected the needed number of newlines
        let mut win = 4096u64.min(size);
        let mut start = size - win;
        loop {
            fseek(&mut f, start);
            let got = fread(&mut f, &mut iobuf()[..win as usize]);
            let nl = iobuf()[..got].iter().filter(|&&c| c == b'\n').count();
            if nl > cnt || start == 0 || win >= 256 * 1024 { break; }
            win = (win * 4).min(size); start = size - win;
        }
        fseek(&mut f, start);
        let got = fread(&mut f, &mut iobuf()[..win as usize]);
        // step back from the end by exactly cnt newlines
        let mut seen = 0usize; let mut from = 0usize;
        let mut i = got;
        while i > 0 {
            i -= 1;
            if iobuf()[i] == b'\n' && i + 1 != got { seen += 1; if seen == cnt { from = i + 1; break; } }
        }
        for i in from..got { putfile(iobuf()[i]); }
    }
    fclose(f);
}
fn parse_n(arg: &str, def: usize) -> (usize, &str) {
    let a = arg.trim();
    if let Some(r) = a.strip_prefix("-n") {
        let r = r.trim_start();
        let (num, rest) = match r.find(' ') { Some(k) => (&r[..k], r[k..].trim()), None => (r, "") };
        return (num.parse::<usize>().unwrap_or(def), rest);
    }
    (def, a)
}

pub fn cmd_hexdump(arg: &str) {
    let mut it = arg.split_whitespace();
    let path = it.next().unwrap_or("");
    let off = it.next().and_then(parse_num).unwrap_or(0);
    let len = it.next().and_then(parse_num).unwrap_or(256) as usize;
    if path.is_empty() { crate::println!("usage: hd <file> [offset] [length]"); return; }
    if !ensure_mounted() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(path, &mut p);
    let mut f = match fopen(&p[..n]) { Some(f) => f, None => { crate::println!("hd: {} — there is no such file", path); return; } };
    fseek(&mut f, off);
    let want = len.min(iobuf().len());
    let got = fread(&mut f, &mut iobuf()[..want]);
    fclose(f);
    let hex = b"0123456789abcdef";
    let mut row = 0usize;
    while row < got {
        crate::print!("{:08x}  ", off as usize + row);
        for c in 0..16 {
            if row + c < got { let v = iobuf()[row + c]; serial::putb(hex[(v >> 4) as usize]); serial::putb(hex[(v & 15) as usize]); }
            else { serial::putb(b' '); serial::putb(b' '); }
            serial::putb(b' ');
            if c == 7 { serial::putb(b' '); }
        }
        serial::putb(b'|');
        for c in 0..16 { if row + c < got { let v = iobuf()[row + c]; serial::putb(if (0x20..0x7f).contains(&v) { v } else { b'.' }); } }
        serial::putb(b'|'); serial::putb(b'\n');
        row += 16;
    }
    if got == 0 { crate::println!("(empty — the offset is past the end of the file)"); }
}
pub fn parse_num(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) { u64::from_str_radix(h, 16).ok() } else { s.parse::<u64>().ok() }
}

pub fn cmd_stat(arg: &str) {
    if arg.is_empty() { crate::println!("usage: stat <path>"); return; }
    if !ensure_mounted() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(arg, &mut p);
    match finfo(&p[..n]) {
        None => crate::println!("stat: {} — there is no such path", arg),
        Some((sz, dir, mode, mt)) => {
            let mut ts = [0u8; 20]; let tn = fmt_time(mt, &mut ts);
            crate::print!("{}: {} the size {} B, the rights {:o}, modified ",
                core::str::from_utf8(&p[..n]).unwrap_or("?"),
                if dir { "a directory," } else { "a file," }, sz, mode & 0o7777);
            puts(&ts[..tn]); serial::putb(b'\n');
        }
    }
}

pub fn cmd_mkdir(arg: &str) {
    if arg.is_empty() { crate::println!("usage: mkdir <path>"); return; }
    if !need_rw() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(arg, &mut p);
    let r = ext2::mkdir(&p[..n], 0o755);
    if r < 0 { crate::println!("mkdir: the error {}", r); } else { crate::println!("the directory {} has been created", core::str::from_utf8(&p[..n]).unwrap_or("?")); }
}

pub fn cmd_rm(arg: &str) {
    let (rec, path) = match arg.trim().strip_prefix("-r") { Some(r) => (true, r.trim()), None => (false, arg.trim()) };
    if path.is_empty() { crate::println!("usage: rm [-r] <path>"); return; }
    if !need_rw() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(path, &mut p);
    if n <= 1 { crate::println!("rm: the root must not be removed"); return; }
    match finfo(&p[..n]) {
        None => crate::println!("rm: {} — there is no such path", path),
        Some((_, true, _, _)) => {
            if !rec { crate::println!("rm: {} — a directory (-r is needed)", path); return; }
            let killed = rm_tree(&mut p, n, 0);
            crate::println!("rm: objects removed {}", killed);
        }
        Some(_) => { let r = ext2::unlink(&p[..n], false); if r < 0 { crate::println!("rm: the error {}", r); } }
    }
}
/// A recursive removal with a depth limit: the console works on the kernel's stack,
/// and an infinite recursion over a forged tree is not "an error of the command".
fn rm_tree(p: &mut [u8; PATH], n: usize, depth: usize) -> usize {
    if depth > 16 { crate::println!("rm: the tree is too deep (>16) — stopped"); return 0; }
    let mut killed = 0usize;
    loop {
        if list_dir(&p[..n]) < 0 { break; }
        let cnt = entries();
        if cnt == 0 { break; }
        // a copy of the names: list_dir reuses the common table at the nested levels
        let mut names = [[0u8; 64]; 32];
        let mut lens = [0usize; 32];
        let mut dirs = [false; 32];
        let take = cnt.min(32);
        for i in 0..take { let (nm, d) = entry(i); let l = nm.len().min(64); names[i][..l].copy_from_slice(&nm[..l]); lens[i] = l; dirs[i] = d; }
        for i in 0..take {
            let mut full = [0u8; PATH];
            let fl = join(&p[..n], &names[i][..lens[i]], &mut full);
            if dirs[i] { killed += rm_tree(&mut full, fl, depth + 1); }
            else if ext2::unlink(&full[..fl], false) == 0 { killed += 1; }
        }
        if cnt <= 32 { break; }
    }
    if ext2::unlink(&p[..n], true) == 0 { killed += 1; }
    killed
}

pub fn cmd_cp(arg: &str) {
    let mut it = arg.split_whitespace();
    let (src, dst) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    if src.is_empty() || dst.is_empty() { crate::println!("usage: cp <from> <to>"); return; }
    if !need_rw() { return; }
    let (mut sp, mut dp) = ([0u8; PATH], [0u8; PATH]);
    let sn = resolve(src, &mut sp);
    let mut dn = resolve(dst, &mut dp);
    // cp a file a directory -> into the directory (as is customary)
    if let Some((_, true, _, _)) = finfo(&dp[..dn]) {
        let base = &sp[..sn];
        let name = match base.iter().rposition(|&c| c == b'/') { Some(k) => &base[k + 1..], None => base };
        let mut j = [0u8; PATH]; dn = join(&dp[..dn], name, &mut j); dp = j;
    }
    let mut f = match fopen(&sp[..sn]) { Some(f) => f, None => { crate::println!("cp: {} — there is no such file", src); return; } };
    let h = ext2::open(&dp[..dn], O_CREAT | O_TRUNC | O_WRONLY, 0o644);
    if h < 0 { crate::println!("cp: {} could not be created (the error {})", core::str::from_utf8(&dp[..dn]).unwrap_or("?"), h); fclose(f); return; }
    let mut total = 0u64;
    loop {
        let got = fread(&mut f, iobuf());
        if got == 0 { break; }
        let w = ext2::write(h, iobuf().as_ptr(), got);
        if w < 0 { crate::println!("cp: a write error {}", w); break; }
        total += w as u64;
    }
    ext2::fsync(h); ext2::close(h); fclose(f);
    crate::println!("cp: {} B copied -> {}", total, core::str::from_utf8(&dp[..dn]).unwrap_or("?"));
}

pub fn cmd_mv(arg: &str) {
    let mut it = arg.split_whitespace();
    let (src, dst) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    if src.is_empty() || dst.is_empty() { crate::println!("usage: mv <from> <to>"); return; }
    if !need_rw() { return; }
    let (mut sp, mut dp) = ([0u8; PATH], [0u8; PATH]);
    let sn = resolve(src, &mut sp);
    let mut dn = resolve(dst, &mut dp);
    if let Some((_, true, _, _)) = finfo(&dp[..dn]) {
        let name = match sp[..sn].iter().rposition(|&c| c == b'/') { Some(k) => &sp[k + 1..sn], None => &sp[..sn] };
        let mut j = [0u8; PATH]; dn = join(&dp[..dn], name, &mut j); dp = j;
    }
    let r = ext2::rename(&sp[..sn], &dp[..dn]);
    if r < 0 { crate::println!("mv: the error {}", r); } else { crate::println!("mv: {} -> {}", src, core::str::from_utf8(&dp[..dn]).unwrap_or("?")); }
}

pub fn cmd_touch(arg: &str) {
    if arg.is_empty() { crate::println!("usage: touch <file>"); return; }
    if !need_rw() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(arg, &mut p);
    let h = ext2::open(&p[..n], O_CREAT | O_WRONLY, 0o644);
    if h < 0 { crate::println!("touch: the error {}", h); return; }
    ext2::close(h);
}

/// `write <file> <text>` / `append <file> <text>` — a one-line edit without an
/// editor: that is how one parameter in a config is changed without opening vi.
pub fn cmd_write(arg: &str, append: bool) {
    let (path, text) = match arg.find(' ') { Some(k) => (&arg[..k], arg[k + 1..].trim_start()), None => (arg.trim(), "") };
    if path.is_empty() { crate::println!("usage: {} <file> <text>", if append { "append" } else { "write" }); return; }
    if !need_rw() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(path, &mut p);
    let fl = O_CREAT | O_WRONLY | if append { O_APPEND } else { O_TRUNC };
    let h = ext2::open(&p[..n], fl, 0o644);
    if h < 0 { crate::println!("{}: the error {}", if append { "append" } else { "write" }, h); return; }
    if append { ext2::lseek(h, 0, 2); }
    let mut line = [0u8; 512];
    let l = text.len().min(511);
    line[..l].copy_from_slice(&text.as_bytes()[..l]); line[l] = b'\n';
    let w = ext2::write(h, line.as_ptr(), l + 1);
    ext2::fsync(h); ext2::close(h);
    crate::println!("{}: {} B -> {}", if append { "append" } else { "write" }, w.max(0), path);
}

pub fn cmd_df() {
    if !ensure_mounted() { return; }
    match backend() {
        Backend::Ext2 => {
            let (bs, total, free, ti, fi) = ext2::statfs();
            let used = total - free;
            crate::println!("the ext2 FS: the block {} B, in total {} MB, occupied {} MB ({}%), free {} MB",
                bs, (total as usize * bs) >> 20, (used as usize * bs) >> 20,
                if total > 0 { used as u64 * 100 / total as u64 } else { 0 }, (free as usize * bs) >> 20);
            crate::println!("the inodes: in total {}, free {}; the journal is {}",
                ti, fi, if ext2::jrn_active() { "on (JBD2)" } else { "off" });
        }
        Backend::Tfs => crate::println!("the TFS file system (read-only): files {}", fs::nfiles()),
        Backend::None => {}
    }
}

pub fn cmd_sync() {
    if ext2::active() { ext2::flush_all(); crate::println!("sync: the FS cache has been flushed to the disk"); }
    else { crate::println!("sync: there is nothing to flush (the FS is read-only)"); }
}

// ── less: viewing page by page ────────────────────────────────────────────────
/// The pagination in the operator's console. It is blocking (it waits for a key), so it is available
/// only from the interactive console — it must not be called from the scheduler's tick.
pub fn cmd_less(arg: &str) {
    if arg.is_empty() { crate::println!("usage: less <file>"); return; }
    if !ensure_mounted() { return; }
    let mut p = [0u8; PATH];
    let n = resolve(arg, &mut p);
    let size = match finfo(&p[..n]) {
        Some((s, false, _, _)) => s,
        Some(_) => { crate::println!("less: {} — that is a directory", arg); return; }
        None => { crate::println!("less: {} — there is no such file", arg); return; }
    };
    let mut f = match fopen(&p[..n]) { Some(f) => f, None => return };
    let page = rows().saturating_sub(1).max(1);
    let mut off = 0u64;
    let mut back: [u64; 128] = [0; 128];
    let mut nback = 0usize;
    loop {
        fseek(&mut f, off);
        let got = fread(&mut f, iobuf());
        if got == 0 { break; }
        // output `page` screen lines, having counted the wrapping of long ones
        let mut used = 0usize; let mut lines = 0usize; let mut col = 0usize;
        while used < got && lines < page {
            let b = iobuf()[used]; used += 1;
            putfile(b);
            if b == b'\n' { lines += 1; col = 0; }
            else { col += 1; if col >= cols() { lines += 1; col = 0; } }
        }
        let next = off + used as u64;
        if next >= size { crate::print!("(the end of the file, {} B) ", size); }
        else { crate::print!("-- {}% -- ", next * 100 / size.max(1)); }
        crate::print!("[space=onwards b=back g=the beginning G=the end q=quit] ");
        let k = getkey();
        crate::print!("\r                                                                       \r");
        match k {
            b'q' | 3 | 27 => break,
            b'b' => { if nback > 0 { nback -= 1; off = back[nback]; } else { off = 0; } }
            b'g' => { nback = 0; off = 0; }
            b'G' => { if nback < back.len() { back[nback] = off; nback += 1; } off = size.saturating_sub(iobuf().len() as u64 / 2); }
            _ => {
                if next >= size { break; }
                if nback < back.len() { back[nback] = off; nback += 1; }
                off = next;
            }
        }
    }
    fclose(f);
}

/// Wait for a key press on COM1 (blocking). The common point for less and vi.
pub fn getkey() -> u8 {
    loop { if let Some(c) = serial::getc() { return c; } core::hint::spin_loop(); }
}

// ── the completion of paths by Tab ────────────────────────────────────────────
/// Complete the last token as a path. It returns (how many bytes to append into
/// `add`, the number of candidates). With one candidate the terminating
/// character is appended too ('/' for a directory, ' ' for a file).
pub fn complete_path(token: &str, add: &mut [u8; 96]) -> (usize, usize) {
    if backend() == Backend::None { return (0, 0); }
    // split into the directory and the name's prefix
    let (dir, pfx) = match token.rfind('/') { Some(k) => (&token[..k + 1], &token[k + 1..]), None => ("", token) };
    let mut dp = [0u8; PATH];
    let dn = resolve(if dir.is_empty() { "." } else { dir }, &mut dp);
    if list_dir(&dp[..dn]) < 0 { return (0, 0); }
    let mut cand = 0usize;
    let mut common = [0u8; 96];
    let mut cl = 0usize;
    let mut only_dir = false;
    for i in 0..entries() {
        let (nm, d) = entry(i);
        if !nm.starts_with(pfx.as_bytes()) { continue; }
        cand += 1;
        if cand == 1 { cl = nm.len().min(96); common[..cl].copy_from_slice(&nm[..cl]); only_dir = d; }
        else {
            let mut k = 0; while k < cl && k < nm.len() && common[k] == nm[k] { k += 1; }
            cl = k;
        }
    }
    if cand == 0 { return (0, 0); }
    let have = pfx.len();
    if cl <= have {
        return (0, cand); // there is no common continuation — the caller will show the list
    }
    let mut n = 0;
    for i in have..cl { add[n] = common[i]; n += 1; }
    if cand == 1 && n < 95 { add[n] = if only_dir { b'/' } else { b' ' }; n += 1; }
    (n, cand)
}
/// The shell's self-test (`talaria.test=shell`). What is checked is what the operator
/// uses blindly: the parsing of paths (including `..` beyond the root), the listing of
/// a directory, creation/copying/removal and the REFUSAL of the modifying commands on a
/// read-only FS.
pub fn selftest() -> bool {
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("shell-test: FAIL {}", what); ok = false; } else { crate::println!("shell-test: ok {}", what); } };
    let mut p = [0u8; PATH];
    // the parsing of paths
    let n = resolve("/a/b/../c", &mut p);
    check(&p[..n] == b"/a/c", "resolve: /a/b/../c -> /a/c");
    let n = resolve("/a/b/../../../..", &mut p);
    check(&p[..n] == b"/", ".. does not go above the root");
    let n = resolve("//x///y//", &mut p);
    check(&p[..n] == b"/x/y", "the extra slashes have been collapsed");
    let n = resolve("", &mut p);
    check(&p[..n] == cwd(), "an empty argument = the current directory");
    // the relative paths from the cwd
    cmd_cd("/");
    check(cwd() == b"/", "cd / -> the root");
    // the time
    let mut ts = [0u8; 20];
    let tn = fmt_time(1_764_000_000, &mut ts);
    check(&ts[..tn] == b"2025-11-24 16:00", "the unix time in a human form");
    if !ext2::active() {
        crate::println!("shell-test: ext2 is not mounted — the file part has been skipped");
        crate::println!("shell-test: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
        return ok;
    }
    // the tree: create, write, list, copy, remove
    cmd_mkdir("/sh");
    check(matches!(finfo(b"/sh"), Some((_, true, _, _))), "mkdir creates a directory");
    cmd_write("/sh/one.txt privet", false);
    check(matches!(finfo(b"/sh/one.txt"), Some((7, false, _, _))), "write puts a line with a newline (7 B)");
    cmd_write("/sh/one.txt esche", true);
    check(matches!(finfo(b"/sh/one.txt"), Some((13, false, _, _))), "append appends to the end (13 B)");
    cmd_cp("/sh/one.txt /sh/two.txt");
    check(matches!(finfo(b"/sh/two.txt"), Some((13, false, _, _))), "cp copies byte for byte");
    cmd_mv("/sh/two.txt /sh/three.txt");
    check(finfo(b"/sh/two.txt").is_none() && finfo(b"/sh/three.txt").is_some(), "mv renames");
    let cnt = list_dir(b"/sh");
    check(cnt == 2, "ls sees both entries");
    let (nm0, _) = entry(0); let (nm1, _) = entry(1);
    check(nm0 == b"one.txt" && nm1 == b"three.txt", "the directory's entries are sorted by name");
    // cd into a relative path
    cmd_cd("/sh");
    check(cwd() == b"/sh", "cd into a subdirectory");
    let n = resolve("one.txt", &mut p);
    check(&p[..n] == b"/sh/one.txt", "a relative path is expanded from the cwd");
    cmd_cd("..");
    check(cwd() == b"/", "cd .. returns upwards");
    // the completion by Tab
    let mut add = [0u8; 96];
    let (an, cand) = complete_path("/sh/o", &mut add);
    check(cand == 1 && &add[..an] == b"ne.txt ", "Tab completes the only path and puts a space");
    let (an2, cand2) = complete_path("/sh/", &mut add);
    check(cand2 == 2 && an2 == 0, "two candidates — there is no common continuation");
    // the recursive removal
    cmd_rm("-r /sh");
    check(finfo(b"/sh").is_none(), "rm -r takes the whole tree down");
    crate::println!("shell-test: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
    ok
}

/// Show the completion candidates (the second Tab).
pub fn show_candidates(token: &str) {
    let (dir, pfx) = match token.rfind('/') { Some(k) => (&token[..k + 1], &token[k + 1..]), None => ("", token) };
    let mut dp = [0u8; PATH];
    let dn = resolve(if dir.is_empty() { "." } else { dir }, &mut dp);
    if list_dir(&dp[..dn]) < 0 { return; }
    serial::putb(b'\n');
    let mut col = 0;
    for i in 0..entries() {
        let (nm, d) = entry(i);
        if !nm.starts_with(pfx.as_bytes()) { continue; }
        puts(nm); if d { serial::putb(b'/'); }
        col += 1;
        if col % 5 == 0 { serial::putb(b'\n'); } else { serial::putb(b' '); serial::putb(b' '); }
    }
    if col % 5 != 0 { serial::putb(b'\n'); }
}
