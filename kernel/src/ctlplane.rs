//! The control plane over TCP: an authenticated `/ctl` route on top of the
//! kernel httpd. It serves the kernel's counters and the cell's telemetry to the operator.
//!
//! A token is mandatory: `?token=<TOKEN>` in the query string or the header
//! `Authorization: Bearer <TOKEN>`. Without a token, or with a wrong one, 401.
//!
//! ## The token is set at boot rather than in the code
//!
//! This used to be `pub const TOKEN: &[u8] = b"talaria-secret"` with a note saying
//! "demo-grade, in production from boot/the manifest". The note stayed a note, and
//! for open source that is the worst possible variant: every build would go into the world
//! with ONE AND THE SAME password, printed in the sources, on a network port.
//!
//! Now the token comes from `talaria.ctltoken=<...>` (the kernel command line,
//! the same place the rest of the configuration comes from: QEMU's `-append`,
//! user-data on EC2). **If no token is set, the control plane is closed entirely**:
//! `/ctl` answers 401 to any request. Deny by default rather than allow by
//! default: a deployment mistake must CLOSE the door, not open it.
//!
//! ## A constant-time comparison
//!
//! The token is compared without an early exit. A byte-by-byte comparison with `return false` on
//! the first difference reports through the response time how many bytes were guessed, and at
//! the latencies Talaria exists for (single-digit microseconds per
//! round trip) such an oracle is measurable over the network, not only locally.

use core::sync::atomic::{AtomicUsize, Ordering};

/// The maximum token length.
const TOKMAX: usize = 64;
static mut TOKEN: [u8; TOKMAX] = [0; TOKMAX];
/// The length of the token that was set; 0 — the control plane is closed.
static TOKLEN: AtomicUsize = AtomicUsize::new(0);

/// Set the token from `talaria.ctltoken=<...>`. An empty value or a missing
/// key leaves the control plane closed.
pub fn set_token(tok: &[u8]) {
    let n = tok.len().min(TOKMAX);
    if n == 0 {
        TOKLEN.store(0, Ordering::SeqCst);
        return;
    }
    let dst: &mut [u8; TOKMAX] = unsafe { &mut *core::ptr::addr_of_mut!(TOKEN) };
    dst[..n].copy_from_slice(&tok[..n]);
    TOKLEN.store(n, Ordering::SeqCst);
}

/// Parse `talaria.ctltoken=` from the kernel command line.
pub fn configure_from_cmdline(cmdline: &str) {
    match crate::cmdline::value(cmdline, "talaria.ctltoken") {
        Some(t) if !t.is_empty() => {
            set_token(t.as_bytes());
            println!("talaria: the /ctl control plane is enabled (a token is set, {} B)", t.len());
        }
        _ => println!("talaria: the /ctl control plane is CLOSED (no talaria.ctltoken=)"),
    }
}

/// Whether a token is set (i.e. whether the route is open at all).
pub fn enabled() -> bool {
    TOKLEN.load(Ordering::SeqCst) != 0
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() { return None; }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Where the token ends in a request: a space, `&`, the end of the line or a quote.
fn tok_end(s: &[u8]) -> usize {
    s.iter()
        .position(|&c| c == b' ' || c == b'&' || c == b'\r' || c == b'\n' || c == b'"')
        .unwrap_or(s.len())
}

/// Is the request addressed to the control plane?
pub fn is_ctl(req: &[u8]) -> bool {
    find(req, b"/ctl").is_some_and(|i| i <= 8) // in the request line (after the method)
}

/// Is the token present and correct (the query `token=` OR `Bearer `)?
///
/// No token set -> always `false`: a closed door rather than an open one.
pub fn authorized(req: &[u8]) -> bool {
    let n = TOKLEN.load(Ordering::SeqCst);
    if n == 0 { return false; }
    let tok: &[u8; TOKMAX] = unsafe { &*core::ptr::addr_of!(TOKEN) };
    let want: &[u8] = &tok[..n];
    // Both places are checked ALWAYS (without a short-circuiting `||`): otherwise the
    // response time would reveal which place the token was accepted from.
    let q = find(req, b"token=").is_some_and(|i| {
        let rest = &req[i + 6..];
        crate::crypto::ct_eq(&rest[..tok_end(rest)], want)
    });
    let b = find(req, b"Bearer ").is_some_and(|i| {
        let rest = &req[i + 7..];
        crate::crypto::ct_eq(&rest[..tok_end(rest)], want)
    });
    q | b
}

fn put(out: &mut [u8], n: &mut usize, s: &[u8]) {
    for &b in s {
        if *n < out.len() { out[*n] = b; *n += 1; }
    }
}

fn put_u64(out: &mut [u8], n: &mut usize, mut v: u64) {
    let mut tmp = [0u8; 20];
    let mut k = 0;
    if v == 0 { put(out, n, b"0"); return; }
    while v > 0 { tmp[k] = b'0' + (v % 10) as u8; v /= 10; k += 1; }
    while k > 0 { k -= 1; if *n < out.len() { out[*n] = tmp[k]; *n += 1; } }
}

/// The body of the `/ctl` response: the kernel's counters + the cell's telemetry.
pub fn body(out: &mut [u8]) -> usize {
    let mut n = 0;
    let fields: [(&[u8], u64); 7] = [
        (b"syscalls=", crate::metrics::syscalls()),
        (b" page_faults=", crate::metrics::page_faults()),
        (b" syscalls_denied=", crate::syscall::filter_denied()),
        (b" pkts=", crate::telemetry::pkts()),
        (b" gaps=", crate::telemetry::gaps()),
        (b" restarts=", crate::telemetry::restarts()),
        (b" free_frames=", crate::mem::free_frames()),
    ];
    put(out, &mut n, b"talaria control-plane OK\n");
    for (name, v) in fields {
        put(out, &mut n, name);
        put_u64(out, &mut n, v);
    }
    put(out, &mut n, b"\n");
    n
}

/// Assemble the full HTTP `/ctl` response. Authorised -> 200 + the body, otherwise -> 401.
pub fn build_response(req: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    if !authorized(req) {
        put(out, &mut n, b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\n\
                           Connection: keep-alive\r\nContent-Length: 13\r\n\r\nunauthorized\n");
        return n;
    }
    let mut tmp = [0u8; 256];
    let bl = body(&mut tmp);
    put(out, &mut n, b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: keep-alive\r\nContent-Length: ");
    put_u64(out, &mut n, bl as u64);
    put(out, &mut n, b"\r\n\r\n");
    put(out, &mut n, &tmp[..bl]);
    n
}

/// The self-test (the gate `talaria.test=ctlauth`): the whole authentication contract.
pub fn selftest() -> bool {
    let mut ok = true;
    let mut check = |cond: bool, what: &str| {
        if !cond { println!("ctlauth: FAILED — {}", what); ok = false; }
    };

    // No token set — the door is closed, and no "correct" token exists.
    set_token(b"");
    check(!enabled(), "without a token the control plane counts as enabled");
    check(!authorized(b"GET /ctl?token=talaria-secret HTTP/1.1\r\n\r\n"),
          "with no token set a request is authorised");
    check(!authorized(b"GET /ctl HTTP/1.1\r\n\r\n"), "an empty request is authorised");

    // A token is set: both places are accepted, and only the exact value.
    set_token(b"s3cr3t-from-cmdline");
    check(enabled(), "with a token set the control plane is off");
    check(authorized(b"GET /ctl?token=s3cr3t-from-cmdline HTTP/1.1\r\n\r\n"), "the query token was rejected");
    check(authorized(b"GET /ctl HTTP/1.1\r\nAuthorization: Bearer s3cr3t-from-cmdline\r\n\r\n"), "the Bearer token was rejected");
    check(!authorized(b"GET /ctl?token=wrong HTTP/1.1\r\n\r\n"), "a wrong token was accepted");
    check(!authorized(b"GET /ctl HTTP/1.1\r\n\r\n"), "a request without a token was accepted");
    // Neither a prefix nor a continuation of the correct token passes: the length is checked.
    check(!authorized(b"GET /ctl?token=s3cr3t HTTP/1.1\r\n\r\n"), "a PREFIX of the token was accepted");
    check(!authorized(b"GET /ctl?token=s3cr3t-from-cmdlineX HTTP/1.1\r\n\r\n"), "a token with a tail was accepted");
    check(is_ctl(b"GET /ctl?token=x HTTP/1.1\r\n"), "/ctl was not recognised");
    check(!is_ctl(b"GET /metrics HTTP/1.1\r\n"), "a foreign path was recognised as /ctl");

    // The responses: 200 with a body on a correct token, 401 otherwise.
    let mut rb = [0u8; 512];
    let rn = build_response(b"GET /ctl?token=s3cr3t-from-cmdline HTTP/1.1\r\nHost: x\r\n\r\n", &mut rb);
    check(rn > 0 && rb.starts_with(b"HTTP/1.1 200 OK"), "a correct token did not give 200");
    let mut rb2 = [0u8; 512];
    let rn2 = build_response(b"GET /ctl?token=bad HTTP/1.1\r\n\r\n", &mut rb2);
    check(rn2 > 0 && rb2.starts_with(b"HTTP/1.1 401"), "a wrong token did not give 401");
    println!("ctlauth: build_response n={} / build_401 n={}", rn, rn2);

    set_token(b""); // do not leave the test token in a live system
    if ok { println!("ctlauth: ALL CHECKS PASSED (the token comes from the cmdline; without a token — refusal)"); }
    else { println!("ctlauth: FAILED"); }
    ok
}
