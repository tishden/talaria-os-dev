//! `edit` — a full-screen modal editor of the operator's console (a subset of vi).
//!
//! WHY. Editing a config in place is what the operator comes onto the node for at all:
//! to change an address, a port, a key of the manifest. While there is no such tool,
//! the only way is to re-upload the whole file from the outside, that is, to have a second
//! working channel exactly when things are bad with the channels. `write`/`append` solve
//! the one-line case, everything else requires an editor.
//!
//! WHY VI AND NOT "A SIMPLE EDITOR". The operator already knows vi: hjkl, i, x, dd,
//! `:wq` are the movements the fingers make by themselves. A set of keys of our own
//! would have to be learnt, and they are learnt at exactly the moment when there is no time.
//!
//! THE BOUNDARIES, DECLARED HONESTLY:
//!   * the buffer is static — `MAXL` lines of `LW` bytes (editing configs, not
//!     editing gigabyte logs);
//!   * a file that did NOT FIT into the buffer (a long line or too many
//!     lines) is opened READ-ONLY: to write the truncated thing over
//!     the original is the worst a editor can do to a config;
//!   * an undo of one step (`u`) — a snapshot of the whole buffer;
//!   * an ANSI terminal (the arrows work, but the canon is hjkl).
//!
//! The editor is blocking (it waits for a key), so it is started only from the
//! interactive console — not from the scheduler's tick.
use crate::{serial, shell};

const MAXL: usize = 400;
const LW: usize = 160;

static mut BUF: [[u8; LW]; MAXL] = [[0; LW]; MAXL];
static mut LEN: [u16; MAXL] = [0; MAXL];
static mut NL: usize = 0;
// the snapshot for `u`: an undo of one step — exactly what is used most often in vi
static mut UBUF: [[u8; LW]; MAXL] = [[0; LW]; MAXL];
static mut ULEN: [u16; MAXL] = [0; MAXL];
static mut UNL: usize = 0;
static mut UVALID: bool = false;

static mut PATHB: [u8; shell::PATH] = [0; shell::PATH];
static mut PATHL: usize = 0;
static mut DIRTY: bool = false;
static mut RDONLY: bool = false;
static mut FINAL_NL: bool = true;

// the position
static mut CY: usize = 0;   // the line in the buffer
static mut CX: usize = 0;   // the column
static mut TOP: usize = 0;  // the first shown line
static mut LOFS: usize = 0; // the horizontal scroll
static mut MSG: [u8; 96] = [0; 96];
static mut MSGL: usize = 0;

static mut FIND: [u8; 64] = [0; 64];
static mut FINDL: usize = 0;

fn buf() -> &'static mut [[u8; LW]; MAXL] { unsafe { &mut *core::ptr::addr_of_mut!(BUF) } }
fn len() -> &'static mut [u16; MAXL] { unsafe { &mut *core::ptr::addr_of_mut!(LEN) } }
fn nl() -> usize { unsafe { NL } }
fn set_nl(v: usize) { unsafe { NL = v; } }
fn line(i: usize) -> &'static [u8] { unsafe { &BUF[i][..LEN[i] as usize] } }
fn llen(i: usize) -> usize { unsafe { LEN[i] as usize } }

fn put(s: &[u8]) { for &b in s { serial::putb(b); } }
/// The path of the file being edited (`:w` without an argument writes exactly here).
fn cur_path() -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(PATHB) as *const u8, PATHL) }
}
fn msg(s: &str) {
    unsafe {
        let n = s.len().min(96);
        MSG[..n].copy_from_slice(&s.as_bytes()[..n]);
        MSGL = n;
    }
}
fn msg_clear() { unsafe { MSGL = 0; } }

// ── the loading/saving ────────────────────────────────────────────────────────
fn load(path: &[u8]) -> bool {
    msg_clear(); // otherwise the report about the PREVIOUS file hangs in the status line
    unsafe {
        NL = 0; CY = 0; CX = 0; TOP = 0; LOFS = 0; DIRTY = false; RDONLY = false; UVALID = false; FINAL_NL = true;
        let n = path.len().min(shell::PATH - 1);
        PATHB[..n].copy_from_slice(&path[..n]); PATHB[n] = 0; PATHL = n;
    }
    let mut f = match shell::fopen(path) {
        Some(f) => f,
        None => {
            // the new file does not exist yet — that is a normal scenario (we create it)
            set_nl(1); len()[0] = 0;
            msg("a new file");
            return true;
        }
    };
    let mut chunk = [0u8; 1024];
    let mut li = 0usize;
    let mut col = 0usize;
    let mut over = false;
    let mut last = b'\n';
    loop {
        let got = shell::fread(&mut f, &mut chunk);
        if got == 0 { break; }
        for i in 0..got {
            let b = chunk[i];
            last = b;
            if b == b'\n' {
                if li < MAXL { len()[li] = col as u16; }
                li += 1; col = 0;
                if li >= MAXL { over = true; break; }
            } else if b == b'\r' {
                // CRLF: we ignore the carriage return — otherwise it would land at the end of the line
            } else {
                if col < LW { buf()[li][col] = b; col += 1; } else { over = true; }
            }
        }
        if li >= MAXL { break; }
    }
    shell::fclose(f);
    if li < MAXL { len()[li] = col as u16; if col > 0 || last != b'\n' { li += 1; } }
    unsafe { FINAL_NL = last == b'\n'; }
    set_nl(li.clamp(1, MAXL));
    if over {
        unsafe { RDONLY = true; }
    msg("the file does not fit into the buffer — READ-ONLY (a write would truncate the original)");
    }
    true
}

fn save(path: &[u8]) -> bool {
    if unsafe { RDONLY } { msg("read-only: the file was truncated at the loading — a write is forbidden"); return false; }
    if !crate::ext2::active() { msg("the FS is read-only (TFS) — it cannot be written"); return false; }
    const O_WRONLY: u64 = 1; const O_CREAT: u64 = 0o100; const O_TRUNC: u64 = 0o1000;
    let h = crate::ext2::open(path, O_CREAT | O_TRUNC | O_WRONLY, 0o644);
    if h < 0 { msg("the file could not be opened for writing"); return false; }
    let mut total = 0usize;
    let n = nl();
    for i in 0..n {
        let l = llen(i);
        if l > 0 { let w = crate::ext2::write(h, line(i).as_ptr(), l); if w < 0 { crate::ext2::close(h); msg("a write error"); return false; }
            total += w as usize; }
        let last = i + 1 == n;
        if !last || unsafe { FINAL_NL } {
            let nlb = b'\n';
            crate::ext2::write(h, &nlb as *const u8, 1);
            total += 1;
        }
    }
    crate::ext2::fsync(h);
    crate::ext2::close(h);
    unsafe { DIRTY = false; }
    let mut m = [0u8; 96];
    let mut k = 0;
    for &b in "written: ".as_bytes() { m[k] = b; k += 1; }
    k += fmt_usize(total, &mut m[k..]);
    for &b in " B".as_bytes() { m[k] = b; k += 1; }
    unsafe { MSG[..k].copy_from_slice(&m[..k]); MSGL = k; }
    true
}
fn fmt_usize(mut v: usize, out: &mut [u8]) -> usize {
    if v == 0 { out[0] = b'0'; return 1; }
    let mut t = [0u8; 20]; let mut n = 0;
    while v > 0 { t[n] = b'0' + (v % 10) as u8; v /= 10; n += 1; }
    for i in 0..n { out[i] = t[n - 1 - i]; }
    n
}

fn snapshot() {
    unsafe {
        let n = NL;
        for i in 0..n { UBUF[i] = BUF[i]; ULEN[i] = LEN[i]; }
        UNL = n; UVALID = true;
    }
}
fn undo() {
    unsafe {
        if !UVALID { msg("there is nothing to undo"); return; }
        for i in 0..UNL { BUF[i] = UBUF[i]; LEN[i] = ULEN[i]; }
        NL = UNL; UVALID = false; DIRTY = true;
        if CY >= NL { CY = NL - 1; }
        clamp_x(false);
        msg("an undo");
    }
}

// ── the drawing ───────────────────────────────────────────────────────────────
fn text_rows() -> usize { shell::rows().saturating_sub(1).max(1) }
fn goto(r: usize, c: usize) { crate::print!("\x1b[{};{}H", r + 1, c + 1); }

fn draw_row(sr: usize) {
    goto(sr, 0);
    let li = unsafe { TOP } + sr;
    if li < nl() {
        let l = line(li);
        let ofs = unsafe { LOFS };
        if ofs < l.len() {
            let end = (ofs + shell::cols()).min(l.len());
            for &b in &l[ofs..end] { serial::putb(if b == b'\t' { b' ' } else if b >= 0x20 { b } else { b'?' }); }
        }
    } else {
        serial::putb(b'~');
    }
    put(b"\x1b[K");
}
fn draw_status() {
    let r = shell::rows() - 1;
    goto(r, 0);
    put(b"\x1b[7m"); // the inversion: the status line is bound to be distinguishable from the text
    unsafe {
        put(&PATHB[..PATHL]);
        if DIRTY { put(b" [+]"); }
        if RDONLY { put(b" [RO]"); }
        let nl = NL;
        crate::print!("  {}/{} lines  col {}  ", CY + 1, nl, CX + 1);
        if MSGL > 0 { put(b"| "); put(&MSG[..MSGL]); }
        else { crate::print!("| i insert  :w write  :q quit  u undo  / search"); }
    }
    put(b"\x1b[K\x1b[0m");
}
fn draw_all() {
    put(b"\x1b[H\x1b[2J");
    for sr in 0..text_rows() { draw_row(sr); }
    draw_status();
    place();
}
fn place() {
    unsafe { goto(CY - TOP, CX - LOFS); }
}
/// Pull the window so that the cursor is visible. true — the window has moved (a full
/// redraw is needed), false — redrawing one line is enough.
fn scroll_fix() -> bool {
    unsafe {
        let h = text_rows();
        let mut moved = false;
        if CY < TOP { TOP = CY; moved = true; }
        if CY >= TOP + h { TOP = CY - h + 1; moved = true; }
        let w = shell::cols();
        if CX < LOFS { LOFS = CX.saturating_sub(8); moved = true; }
        if CX >= LOFS + w { LOFS = CX - w + 1; moved = true; }
        moved
    }
}
fn clamp_x(insert: bool) {
    unsafe {
        let l = llen(CY);
        let max = if insert { l } else { l.saturating_sub(1) };
        if CX > max { CX = max; }
    }
}

// ── the edits of the buffer ───────────────────────────────────────────────────
fn ins_char(b: u8) {
    unsafe {
        let l = llen(CY);
        if l + 1 >= LW { msg("the line is of the maximal length"); return; }
        let row = &mut BUF[CY];
        let mut i = l;
        while i > CX { row[i] = row[i - 1]; i -= 1; }
        row[CX] = b;
        LEN[CY] = (l + 1) as u16;
        CX += 1; DIRTY = true;
    }
}
fn del_char() {
    unsafe {
        let l = llen(CY);
        if CX >= l { return; }
        let row = &mut BUF[CY];
        for i in CX..l - 1 { row[i] = row[i + 1]; }
        LEN[CY] = (l - 1) as u16;
        DIRTY = true;
    }
}
fn split_line() {
    unsafe {
        if NL + 1 >= MAXL { msg("no more lines fit"); return; }
        let mut i = NL;
        while i > CY + 1 { BUF[i] = BUF[i - 1]; LEN[i] = LEN[i - 1]; i -= 1; }
        let l = llen(CY);
        let tail = l - CX;
        let (src, dst) = (CX, CY + 1);
        for k in 0..tail { BUF[dst][k] = BUF[CY][src + k]; }
        LEN[dst] = tail as u16;
        LEN[CY] = CX as u16;
        NL += 1; CY += 1; CX = 0; DIRTY = true;
    }
}
fn join_line() {
    unsafe {
        if CY + 1 >= NL { return; }
        let a = llen(CY); let b = llen(CY + 1);
        if a + b >= LW { msg("the joined line does not fit into a line"); return; }
        for k in 0..b { BUF[CY][a + k] = BUF[CY + 1][k]; }
        LEN[CY] = (a + b) as u16;
        for i in CY + 1..NL - 1 { BUF[i] = BUF[i + 1]; LEN[i] = LEN[i + 1]; }
        NL -= 1; DIRTY = true;
    }
}
fn del_line() {
    unsafe {
        if NL <= 1 { LEN[0] = 0; CX = 0; DIRTY = true; return; }
        for i in CY..NL - 1 { BUF[i] = BUF[i + 1]; LEN[i] = LEN[i + 1]; }
        NL -= 1;
        if CY >= NL { CY = NL - 1; }
        CX = 0; DIRTY = true;
    }
}
fn open_line(below: bool) {
    unsafe {
        if NL + 1 >= MAXL { msg("no more lines fit"); return; }
        let at = if below { CY + 1 } else { CY };
        let mut i = NL;
        while i > at { BUF[i] = BUF[i - 1]; LEN[i] = LEN[i - 1]; i -= 1; }
        LEN[at] = 0; NL += 1; CY = at; CX = 0; DIRTY = true;
    }
}

// ── the search ────────────────────────────────────────────────────────────────
fn search_next() {
    unsafe {
        if FINDL == 0 { msg("no pattern is set (/)"); return; }
        let pat = &FIND[..FINDL];
        let start = CY;
        for k in 1..=NL {
            let i = (start + k) % NL;
            let l = line(i);
            if l.len() >= pat.len() {
                for p in 0..=l.len() - pat.len() {
                    if &l[p..p + pat.len()] == pat {
                        CY = i; CX = p; msg_clear(); return;
                    }
                }
            }
        }
        msg("not found");
    }
}

/// Read a line at the bottom of the screen (for `:` and `/`). None — cancelled (ESC).
fn read_bottom(lead: u8, out: &mut [u8; 64]) -> Option<usize> {
    let r = shell::rows() - 1;
    goto(r, 0);
    put(b"\x1b[0m");
    serial::putb(lead);
    put(b"\x1b[K");
    let mut n = 0usize;
    loop {
        let c = shell::getkey();
        match c {
            b'\r' | b'\n' => return Some(n),
            27 => return None,
            8 | 127 => { if n > 0 { n -= 1; put(b"\x08 \x08"); } else { return None; } }
            c if c >= 0x20 && n < 63 => { out[n] = c; n += 1; serial::putb(c); }
            _ => {}
        }
    }
}

// ── the main loop ─────────────────────────────────────────────────────────────
pub fn run(arg: &str) {
    if arg.is_empty() { crate::println!("usage: edit <file>"); return; }
    if !shell::ensure_mounted() { return; }
    let mut p = [0u8; shell::PATH];
    let n = shell::resolve(arg, &mut p);
    if let Some((_, true, _, _)) = shell::finfo(&p[..n]) { crate::println!("edit: {} — that is a directory", arg); return; }
    if !load(&p[..n]) { return; }
    draw_all();
    let mut insert = false;
    let mut pending = 0u8; // the wait for a second key: 'd' (dd), 'g' (gg), 'r'
    loop {
        let c = shell::getkey();
        let mut full = false;
        let old_cy = unsafe { CY };
        msg_clear();
        if insert {
            match c {
                27 => { insert = false; unsafe { CX = CX.saturating_sub(1); } }
                b'\r' | b'\n' => { split_line(); full = true; }
                8 | 127 => unsafe {
                    if CX > 0 { CX -= 1; del_char(); }
                    else if CY > 0 { let prev = llen(CY - 1); CY -= 1; CX = prev; join_line(); full = true; }
                },
                c if c >= 0x20 && c != 0x7f => ins_char(c),
                _ => {}
            }
        } else if pending != 0 {
            let op = pending; pending = 0;
            match (op, c) {
                (b'd', b'd') => { snapshot(); del_line(); full = true; }
                (b'g', b'g') => unsafe { CY = 0; CX = 0; },
                (b'r', ch) if ch >= 0x20 => unsafe { snapshot(); if CX < llen(CY) { BUF[CY][CX] = ch; DIRTY = true; } },
                _ => {}
            }
        } else {
            match c {
                b'h' => unsafe { CX = CX.saturating_sub(1); },
                b'l' => unsafe { if CX + 1 < llen(CY) { CX += 1; } },
                b'j' => unsafe { if CY + 1 < NL { CY += 1; clamp_x(false); } },
                b'k' => unsafe { if CY > 0 { CY -= 1; clamp_x(false); } },
                b'0' => unsafe { CX = 0; },
                b'^' => unsafe { let l = line(CY); CX = l.iter().position(|&b| b != b' ' && b != b'\t').unwrap_or(0); },
                b'$' => unsafe { CX = llen(CY).saturating_sub(1); },
                b'w' => unsafe { // the next word
                    let l = line(CY);
                    let mut i = CX;
                    while i < l.len() && l[i] != b' ' { i += 1; }
                    while i < l.len() && l[i] == b' ' { i += 1; }
                    if i >= l.len() && CY + 1 < NL { CY += 1; CX = 0; } else { CX = i.min(llen(CY).saturating_sub(1)); }
                },
                b'b' => unsafe {
                    let l = line(CY);
                    let mut i = CX;
                    while i > 0 && l[i - 1] == b' ' { i -= 1; }
                    while i > 0 && l[i - 1] != b' ' { i -= 1; }
                    if i == 0 && CX == 0 && CY > 0 { CY -= 1; CX = llen(CY).saturating_sub(1); } else { CX = i; }
                },
                b'G' => unsafe { CY = NL - 1; clamp_x(false); },
                b'g' => pending = b'g',
                b'd' => pending = b'd',
                b'r' => pending = b'r',
                b'x' => { snapshot(); del_char(); clamp_x(false); }
                b'D' => { snapshot(); unsafe { LEN[CY] = CX as u16; DIRTY = true; } }
                b'J' => { snapshot(); join_line(); full = true; }
                b'u' => { undo(); full = true; }
                b'i' => insert = true,
                b'a' => unsafe { if llen(CY) > 0 { CX += 1; } insert = true; clamp_x(true); },
                b'I' => unsafe { CX = 0; insert = true; },
                b'A' => unsafe { CX = llen(CY); insert = true; },
                b'o' => { snapshot(); open_line(true); insert = true; full = true; }
                b'O' => { snapshot(); open_line(false); insert = true; full = true; }
                b'n' => { search_next(); }
                b'/' => {
                    let mut q = [0u8; 64];
                    match read_bottom(b'/', &mut q) {
                        Some(l) if l > 0 => unsafe { FIND[..l].copy_from_slice(&q[..l]); FINDL = l; search_next(); }
                        _ => {}
                    }
                    full = true;
                }
                b':' => {
                    let mut q = [0u8; 64];
                    let l = match read_bottom(b':', &mut q) { Some(l) => l, None => { draw_all(); continue; } };
                    let cmd = core::str::from_utf8(&q[..l]).unwrap_or("").trim();
                    match cmd {
                        "w" => { save(cur_path()); }
                        "q" => {
                            if unsafe { DIRTY } { msg("there are unsaved edits — :q! to quit without writing, :wq to write"); }
                            else { leave(); return; }
                        }
                        "q!" => { leave(); return; }
                        "wq" | "x" => { if save(cur_path()) { leave(); return; } }
                        other => {
                            if let Some(f) = other.strip_prefix("w ") {
                                let mut np = [0u8; shell::PATH];
                                let nn = shell::resolve(f.trim(), &mut np);
                                if save(&np[..nn]) { unsafe { PATHB[..nn].copy_from_slice(&np[..nn]); PATHL = nn; } }
                            } else if let Ok(num) = other.parse::<usize>() {
                                unsafe { CY = num.saturating_sub(1).min(NL - 1); CX = 0; }
                            } else { msg("an unknown command: :w :q :q! :wq :w <file> :<line number>"); }
                        }
                    }
                    full = true;
                }
                b'Z' => { // ZZ — write and quit
                    if shell::getkey() == b'Z' {
                        if save(cur_path()) { leave(); return; }
                        full = true;
                    }
                }
                // The arrows. The inner `if` stays an `if` on purpose: as a match GUARD it
                // would CONSUME a key while deciding which arm matches, and a guard that
                // fails falls through to the next arm — the key would be eaten silently the
                // day an arm is added below.
                #[allow(clippy::collapsible_match)]
                27 => {
                    if shell::getkey() == b'[' {
                        match shell::getkey() {
                            b'A' => unsafe { if CY > 0 { CY -= 1; clamp_x(false); } },
                            b'B' => unsafe { if CY + 1 < NL { CY += 1; clamp_x(false); } },
                            b'C' => unsafe { if CX + 1 < llen(CY) { CX += 1; } },
                            b'D' => unsafe { CX = CX.saturating_sub(1); },
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if scroll_fix() || full { draw_all(); }
        else {
            if old_cy != unsafe { CY } { draw_row(old_cy - unsafe { TOP }); }
            draw_row(unsafe { CY - TOP });
            draw_status();
            place();
        }
    }
}
fn leave() {
    let r = shell::rows() - 1;
    goto(r, 0);
    put(b"\x1b[0m\x1b[K\n");
}

/// The editor's self-test (`talaria.test=edit`): run an editing scenario on a live
/// ext2 and check the file's contents after the write. The keys are fed by the same path
/// the operator feeds them by — through the handlers rather than past them.
pub fn selftest() -> bool {
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("edit-test: FAIL {}", what); ok = false; } else { crate::println!("edit-test: ok {}", what);
        } };
    // prepare the file
    const O_WRONLY: u64 = 1; const O_CREAT: u64 = 0o100; const O_TRUNC: u64 = 0o1000;
    let h = crate::ext2::open(b"/edit.txt", O_CREAT | O_TRUNC | O_WRONLY, 0o644);
    if h < 0 { crate::println!("edit-test: /edit.txt could not be created"); return false; }
    let src = b"alpha\nbravo\ncharlie\n";
    crate::ext2::write(h, src.as_ptr(), src.len());
    crate::ext2::fsync(h); crate::ext2::close(h);

    check(load(b"/edit.txt"), "the loading of a file");
    check(nl() == 3, "the parsing into lines (3)");
    check(line(1) == b"bravo", "the second line = bravo");
    // dd on the first line
    unsafe { CY = 0; CX = 0; }
    snapshot(); del_line();
    check(nl() == 2 && line(0) == b"bravo", "dd deletes a line");
    undo();
    check(nl() == 3 && line(0) == b"alpha", "u undoes the deletion");
    // an edit: append "-1" to the end of the first line
    unsafe { CY = 0; CX = llen(0); }
    ins_char(b'-'); ins_char(b'1');
    check(line(0) == b"alpha-1", "the insertion of characters at the end of a line");
    // cut a line and join it back
    unsafe { CY = 1; CX = 2; }
    split_line();
    check(nl() == 4 && line(1) == b"br" && line(2) == b"avo", "the cutting of a line (Enter in insert)");
    unsafe { CY = 1; CX = 2; }
    join_line();
    check(nl() == 3 && line(1) == b"bravo", "the joining of lines (J)");
    // the search
    unsafe { FIND[..4].copy_from_slice(b"char"); FINDL = 4; CY = 0; }
    search_next();
    check(unsafe { CY } == 2, "the search /char finds the third line");
    // the write and a re-read
    check(save(b"/edit.txt"), "the writing of the file");
    let mut back = [0u8; 64];
    let got = crate::ext2::read_all(b"/edit.txt", &mut back);
    check(&back[..got] == b"alpha-1\nbravo\ncharlie\n", "the file on the disk matches the buffer");
    // the protection from truncation: a file longer than the buffer is opened read-only
    let h = crate::ext2::open(b"/long.txt", O_CREAT | O_TRUNC | O_WRONLY, 0o644);
    if h >= 0 {
        let mut big = [b'x'; LW + 40];
        big[LW + 39] = b'\n';
        crate::ext2::write(h, big.as_ptr(), big.len());
        crate::ext2::fsync(h); crate::ext2::close(h);
        load(b"/long.txt");
        check(unsafe { RDONLY }, "a line that is too long -> the read-only mode");
        check(!save(b"/long.txt"), "the writing of a truncated file is REJECTED");
    }
    crate::println!("edit-test: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
    ok
}
