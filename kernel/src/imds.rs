//! A mini client of the EC2 metadata service (IMDSv2, PLAN-V1.1 §3.3): 169.254.169.254,
//! PUT /latest/api/token -> GET instance-id/hostname. Optional: it is switched on by the
//! cmdline `talaria.imds=1` (after netsys has come up; the route goes through the default
//! route, as in a VPC). The results go into statics for the operator's side (/ctl, the banner).
use crate::netsys;

const IMDS_IP: [u8; 4] = [169, 254, 169, 254];
static mut INSTANCE_ID: [u8; 32] = [0; 32];
static mut HOSTNAME: [u8; 64] = [0; 64];

fn sockaddr(ip: [u8; 4], port: u16) -> [u8; 16] {
    let mut sa = [0u8; 16];
    sa[0] = 2; // AF_INET
    sa[2] = (port >> 8) as u8; sa[3] = port as u8;
    sa[4..8].copy_from_slice(&ip);
    sa
}

/// An HTTP request (Connection: close), the answer's body -> out. None — a network/HTTP error.
fn http(req: &[u8], out: &mut [u8]) -> Option<usize> {
    let fd = netsys::socket(1); // SOCK_STREAM
    if fd < 0 { return None; }
    let sa = sockaddr(IMDS_IP, 80);
    if netsys::connect(fd, sa.as_ptr()) < 0 { netsys::close(fd); return None; }
    let mut off = 0;
    while off < req.len() {
        let w = netsys::write(fd, unsafe { req.as_ptr().add(off) }, req.len() - off, 0);
        if w <= 0 { netsys::close(fd); return None; }
        off += w as usize;
    }
    // read until the peer closes (Connection: close)
    let mut buf = [0u8; 1024];
    let mut n = 0usize;
    loop {
        let r = netsys::read(fd, unsafe { buf.as_mut_ptr().add(n) }, buf.len() - n, 0);
        if r <= 0 { break; }
        n += r as usize;
        if n == buf.len() { break; }
    }
    netsys::close(fd);
    let resp = &buf[..n];
    // the status 200 + the body after \r\n\r\n
    if n < 12 || &resp[9..12] != b"200" { return None; }
    let body_at = resp.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let body = &resp[body_at..];
    let m = body.len().min(out.len());
    out[..m].copy_from_slice(&body[..m]);
    Some(m)
}

fn get_meta(token: &[u8], path: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut req = [0u8; 256];
    let mut w = 0;
    for part in [b"GET " as &[u8], path, b" HTTP/1.1\r\nHost: 169.254.169.254\r\nX-aws-ec2-metadata-token: ", token, b"\r\nConnection: close\r\n\r\n"] {
        if w + part.len() > req.len() { return None; }
        req[w..w + part.len()].copy_from_slice(part); w += part.len();
    }
    http(&req[..w], out)
}

/// Fetch the instance-id and the hostname; print them into the serial line. Call it at talaria.imds=1
/// A4: the IMDSv2 token (PUT /latest/api/token). It returns its length (0 — it was not given).
///
/// The retries are bounded by TIME rather than by the number of attempts. There used to stand exactly 8
/// attempts, and that nearly cost a whole session: on a fresh instance the metadata
/// service answered only on the ninth (~90 s — every failed connect costs
/// a 10 s timeout), the user-data arrived empty, the kernel silently booted with the
/// CANONICAL cmdline, and the whole experiment went in the wrong mode — that looked
/// like "the stand does not answer". Counting by time is more honest: the price of one attempt
/// depends on what exactly the connect ended with.
fn token_get(token: &mut [u8; 128]) -> usize {
    let hz = crate::tsc::calibrate_cached();
    let start = crate::tsc::rdtsc();
    let budget = 150 * hz;   // the ceiling of the wait for the metadata, s
    let mut tries = 0u32;
    loop {
        if tries > 0 {
            let t0 = crate::tsc::rdtsc();
            while crate::tsc::rdtsc() - t0 < hz / 2 { netsys::poll(); core::hint::spin_loop(); }
        }
        tries += 1;
        if let Some(nn) = http(b"PUT /latest/api/token HTTP/1.1\r\nHost: 169.254.169.254\r\nX-aws-ec2-metadata-token-ttl-seconds: 21600\r\nConnection: close\r\n\r\n", token) {
            if nn > 0 {
                if tries > 1 {
                    crate::println!("imds: the token was obtained on attempt {} ({} s of waiting)",
                        tries, (crate::tsc::rdtsc() - start) / hz);
                }
                return nn;
            }
        }
        if crate::tsc::rdtsc() - start > budget {
            crate::println!("imds: the token was not obtained in {} s ({} attempts)", (crate::tsc::rdtsc() - start) / hz, tries);
            return 0;
        }
    }
}

/// after netsys::init (there is already an address — a static one or a DHCP lease).
pub fn fetch() {
    let mut token = [0u8; 128];
    let tn = token_get(&mut token);
    if tn == 0 { crate::println!("imds: it is unavailable (there is no answer to the PUT token)"); return; }
    let tok = &token[..tn];
    let (mut id, mut hn) = ([0u8; 32], [0u8; 64]);
    let idn = get_meta(tok, b"/latest/meta-data/instance-id", &mut id).unwrap_or(0);
    let hnn = get_meta(tok, b"/latest/meta-data/hostname", &mut hn).unwrap_or(0);
    unsafe { INSTANCE_ID[..idn].copy_from_slice(&id[..idn]); HOSTNAME[..hnn].copy_from_slice(&hn[..hnn]); }
    crate::println!("imds: instance-id={} hostname={}",
        core::str::from_utf8(&id[..idn]).unwrap_or("?"),
        core::str::from_utf8(&hn[..hnn]).unwrap_or("?"));
}

/// §0.1: the instance's user-data (`/latest/user-data`) — the cmdline's tail for one AMI
/// for all the experiments. It returns the number of bytes (0 — there is no user-data/it is unavailable).
/// The token is taken by the same PUT as in fetch(): it is called separately, because
/// the user-data is needed EARLY (before the mode is chosen), while fetch() is the operator's reference.
pub fn user_data(out: &mut [u8]) -> usize {
    let mut token = [0u8; 128];
    let tn = token_get(&mut token);
    if tn == 0 { crate::println!("imds: the user-data is unavailable (there is no token)"); return 0; }
    // A 404 (the user-data is not set) http() gives out as None — that is NOT an error, but
    // a one-off failure looks the same. We retry: after a stop/start of an instance the service
    // answers the PUT of the token earlier than it starts giving out the user-data, and
    // a single attempt silently gave "the user-data is empty" (the experiment went
    // with an empty cmdline tail — the test's phase simply was not carried out).
    let mut n = 0usize;
    for attempt in 0..8 {
        if attempt > 0 {
            let hz = crate::tsc::calibrate_cached();
            let t0 = crate::tsc::rdtsc();
            while crate::tsc::rdtsc() - t0 < hz / 2 { netsys::poll(); core::hint::spin_loop(); }
        }
        n = get_meta(&token[..tn], b"/latest/user-data", out).unwrap_or(0);
        if n > 0 { break; }
    }
    // trim by a newline/garbage: the cmdline is one line
    let mut w = 0;
    while w < n && out[w] >= 0x20 && out[w] < 0x7F { w += 1; }
    w
}

