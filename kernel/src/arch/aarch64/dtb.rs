//! The flattened device tree (FDT, "the DTB"): the one description of the machine a
//! loader hands an arm64 kernel when there is no ACPI — QEMU `virt` with `-kernel`,
//! every board without UEFI. The kernel needs a dozen facts out of it: where the RAM
//! is, the command line, the UART, the GIC's frames, the timer's PPIs, how a core is
//! started (PSCI through `hvc` or `smc`), which cores exist, the virtio-mmio windows
//! and the PCIe ECAM. This module walks the tree once and returns exactly those facts
//! in a fixed-size `Summary`; everything else in the tree is skipped.
//!
//! Pure bytes in, a struct out: no allocation, no device, no `crate::`. It is compiled
//! for the host too (`kernel-unit`) and judged against a device tree dumped from the
//! stand's own QEMU (`kernel-unit/fixtures/`), plus malformed inputs — a truncated or
//! lying tree returns an error instead of indexing past the buffer.
//!
//! The format (the devicetree specification, chapter 5): a 40-byte big-endian header;
//! a structure block of 4-byte tokens — `BEGIN_NODE` + a NUL-terminated name padded
//! to 4, `PROP` + `len` + `nameoff` + the value padded to 4, `END_NODE`, `NOP`, `END`;
//! a strings block that `nameoff` indexes. Addresses in `reg` are `#address-cells`
//! + `#size-cells` big-endian words, and the cell counts are a property of the PARENT
//! node (default 2 and 1).

const MAGIC: u32 = 0xD00D_FEED;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

/// The most of each thing the summary keeps. A machine with more RAM ranges, cores or
/// virtio windows is not an error: the extra ones are counted in `*_seen` and dropped.
pub const MAX_MEM: usize = 8;
pub const MAX_CPU: usize = 64;
pub const MAX_VIRTIO: usize = 32;
/// Nodes nested deeper than this are walked but not classified (nothing we need is
/// deeper than `/intc/its` or `/cpus/cpu-map/socket/cluster/core`).
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a device tree at all (the first word is not `0xD00DFEED`).
    BadMagic,
    /// The header or a block points past the end of the bytes given.
    Truncated,
    /// A token that is not one of the five, or a node that never ends.
    BadStructure,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Range { pub base: u64, pub size: u64 }

/// A byte range inside the tree (for the strings the kernel wants to keep pointing at:
/// the command line lives in the loader's memory and is copied by the caller).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Span { pub off: usize, pub len: usize }

/// How a secondary core is started (the `psci` node's `method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Psci { Hvc, Smc }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub mem: [Range; MAX_MEM],
    pub nmem: usize,
    pub mem_seen: usize,
    /// `/chosen/bootargs`, without the NUL.
    pub bootargs: Option<Span>,
    /// The PL011 named by `/chosen/stdout-path`, else the first PL011 in the tree.
    pub uart: Option<Range>,
    /// The PL031 real-time clock (`arm,pl031`): the wall clock's base at boot.
    pub rtc: Option<Range>,
    /// The host bridge's 32-bit memory window (`ranges` of `pci-host-ecam-generic`), as CPU
    /// addresses: the BARs nobody assigned are taken from it.
    pub pci_mem32: Option<Range>,
    /// The GIC v3 distributor and the redistributor region (`reg` of `arm,gic-v3`).
    pub gicd: Option<Range>,
    pub gicr: Option<Range>,
    /// The ITS frame (`arm,gic-v3-its`, a child of the GIC).
    pub its: Option<Range>,
    /// The generic timer's PPIs as INTIDs, in the node's order: the secure physical, the
    /// non-secure physical, the virtual, the hypervisor. 0 = not given.
    pub timer_ppi: [u32; 4],
    pub psci: Option<Psci>,
    /// The `reg` (the MPIDR affinity) of every `device_type = "cpu"` node.
    pub cpus: [u64; MAX_CPU],
    pub ncpu: usize,
    pub cpu_seen: usize,
    /// `virtio,mmio` windows: the base and the SPI as an INTID (32 + the `interrupts` number).
    pub virtio: [(u64, u32); MAX_VIRTIO],
    pub nvirtio: usize,
    pub virtio_seen: usize,
    /// The ECAM window of `pci-host-ecam-generic`.
    pub ecam: Option<Range>,
}

// `Default` cannot be derived: the arrays are longer than 32 (the cpu table).
impl Default for Summary {
    fn default() -> Self {
        Summary {
            mem: [Range::default(); MAX_MEM], nmem: 0, mem_seen: 0, bootargs: None, uart: None, rtc: None, pci_mem32: None,
            gicd: None, gicr: None, its: None, timer_ppi: [0; 4], psci: None,
            cpus: [0; MAX_CPU], ncpu: 0, cpu_seen: 0, virtio: [(0, 0); MAX_VIRTIO], nvirtio: 0, virtio_seen: 0,
            ecam: None,
        }
    }
}

/// What one node accumulates between its `BEGIN_NODE` and its `END_NODE`; it is
/// classified when it ends, because the properties come in any order (QEMU writes
/// `compatible` last).
#[derive(Clone, Copy, Default)]
struct Node {
    name: Span,
    reg: Option<Span>,
    ranges: Option<Span>,
    interrupts: Option<Span>,
    compatible: Option<Span>,
    device_type: Option<Span>,
    method: Option<Span>,
    /// The cells that apply to THIS node's children.
    child_addr_cells: u32,
    child_size_cells: u32,
}

fn be32(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// `cells` big-endian words at `off` as one number (1 or 2 cells; more are truncated to
/// the low 64 bits, which is what every real tree means).
fn cells(d: &[u8], off: usize, n: u32) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..n as usize {
        v = (v << 32) | be32(d, off + 4 * i)? as u64;
    }
    Some(v)
}

fn span_bytes(d: &[u8], s: Span) -> &[u8] { d.get(s.off..s.off + s.len).unwrap_or(&[]) }

/// A NUL-terminated string at `off`, without the NUL; `None` if there is no NUL before `limit`.
fn cstr(d: &[u8], off: usize, limit: usize) -> Option<Span> {
    let end = limit.min(d.len());
    let mut i = off;
    while i < end {
        if d[i] == 0 { return Some(Span { off, len: i - off }); }
        i += 1;
    }
    None
}

/// Is `needle` one of the NUL-separated strings of a `compatible` list?
fn compat_has(d: &[u8], c: Option<Span>, needle: &[u8]) -> bool {
    let Some(c) = c else { return false; };
    span_bytes(d, c).split(|&b| b == 0).any(|s| s == needle)
}

fn str_is(d: &[u8], s: Option<Span>, want: &[u8]) -> bool {
    match s {
        Some(s) => {
            let b = span_bytes(d, s);
            let b = b.strip_suffix(&[0]).unwrap_or(b);
            b == want
        }
        None => false,
    }
}

/// The `i`-th (base, size) pair of a `reg` under the parent's cells.
fn reg_pair(d: &[u8], reg: Span, ac: u32, sc: u32, i: usize) -> Option<Range> {
    let stride = 4 * (ac + sc) as usize;
    if stride == 0 { return None; }
    let off = reg.off + i * stride;
    if off + stride > reg.off + reg.len { return None; }
    Some(Range { base: cells(d, off, ac)?, size: cells(d, off + 4 * ac as usize, sc)? })
}

/// Does `path` (a `stdout-path`, possibly with a `:baud` suffix) end in this node's name?
fn path_names(d: &[u8], path: Span, name: Span) -> bool {
    let p = span_bytes(d, path);
    let p = p.strip_suffix(&[0]).unwrap_or(p);
    let p = match p.iter().position(|&b| b == b':') { Some(i) => &p[..i], None => p };
    let last = match p.iter().rposition(|&b| b == b'/') { Some(i) => &p[i + 1..], None => p };
    last == span_bytes(d, name)
}

/// Walk the tree and collect the summary. `d` is the whole DTB, starting at its header.
pub fn parse(d: &[u8]) -> Result<Summary, Error> {
    if be32(d, 0).ok_or(Error::Truncated)? != MAGIC { return Err(Error::BadMagic); }
    let total = be32(d, 4).ok_or(Error::Truncated)? as usize;
    let off_struct = be32(d, 8).ok_or(Error::Truncated)? as usize;
    let off_strings = be32(d, 12).ok_or(Error::Truncated)? as usize;
    let size_strings = be32(d, 32).ok_or(Error::Truncated)? as usize;
    let size_struct = be32(d, 36).ok_or(Error::Truncated)? as usize;
    if total > d.len() || off_struct + size_struct > total || off_strings + size_strings > total {
        return Err(Error::Truncated);
    }
    let struct_end = off_struct + size_struct;
    let strings_end = off_strings + size_strings;

    let mut out = Summary::default();
    let mut stack = [Node::default(); MAX_DEPTH];
    // The root's parent: the specification's defaults for a node without `#*-cells`.
    let root_parent = Node { child_addr_cells: 2, child_size_cells: 1, ..Node::default() };
    let mut depth: usize = 0; // the number of open nodes
    let mut stdout_path: Option<Span> = None;
    let mut first_pl011: Option<Range> = None;
    let mut chosen_uart: Option<Range> = None;

    let mut p = off_struct;
    loop {
        let tok = be32(d, p).ok_or(Error::Truncated)?;
        if p + 4 > struct_end { return Err(Error::Truncated); }
        p += 4;
        match tok {
            BEGIN_NODE => {
                let name = cstr(d, p, struct_end).ok_or(Error::Truncated)?;
                p = (p + name.len + 1 + 3) & !3;
                if depth >= MAX_DEPTH { return Err(Error::BadStructure); }
                // The defaults for THIS node's children; its own properties may override.
                stack[depth] = Node { name, child_addr_cells: 2, child_size_cells: 1, ..Node::default() };
                depth += 1;
            }
            PROP => {
                let len = be32(d, p).ok_or(Error::Truncated)? as usize;
                let nameoff = be32(d, p + 4).ok_or(Error::Truncated)? as usize;
                p += 8;
                if p + len > struct_end { return Err(Error::Truncated); }
                let val = Span { off: p, len };
                p = (p + len + 3) & !3;
                if depth == 0 { return Err(Error::BadStructure); }
                let pname = cstr(d, off_strings + nameoff, strings_end).ok_or(Error::Truncated)?;
                let pname = span_bytes(d, pname);
                let node = &mut stack[depth - 1];
                match pname {
                    b"reg" => node.reg = Some(val),
                    b"ranges" => node.ranges = Some(val),
                    b"interrupts" => node.interrupts = Some(val),
                    b"compatible" => node.compatible = Some(val),
                    b"device_type" => node.device_type = Some(val),
                    b"method" => node.method = Some(val),
                    b"#address-cells" => node.child_addr_cells = be32(d, val.off).ok_or(Error::Truncated)?,
                    b"#size-cells" => node.child_size_cells = be32(d, val.off).ok_or(Error::Truncated)?,
                    b"bootargs" if depth == 2 && span_bytes(d, node.name) == b"chosen" => {
                        let s = span_bytes(d, val);
                        let n = s.iter().position(|&b| b == 0).unwrap_or(s.len());
                        out.bootargs = Some(Span { off: val.off, len: n });
                    }
                    b"stdout-path" if depth == 2 && span_bytes(d, node.name) == b"chosen" => stdout_path = Some(val),
                    _ => {}
                }
            }
            END_NODE => {
                if depth == 0 { return Err(Error::BadStructure); }
                depth -= 1;
                let node = stack[depth];
                let parent = if depth == 0 { root_parent } else { stack[depth - 1] };
                classify(d, &node, &parent, &mut out, &mut first_pl011, &mut chosen_uart, stdout_path);
            }
            NOP => {}
            END => break,
            _ => return Err(Error::BadStructure),
        }
    }
    if depth != 0 { return Err(Error::BadStructure); }
    out.uart = chosen_uart.or(first_pl011);
    Ok(out)
}

/// A node has ended: does it carry one of the facts the kernel wants?
fn classify(d: &[u8], node: &Node, parent: &Node, out: &mut Summary,
            first_pl011: &mut Option<Range>, chosen_uart: &mut Option<Range>, stdout_path: Option<Span>) {
    let (ac, sc) = (parent.child_addr_cells, parent.child_size_cells);
    let reg0 = |i| node.reg.and_then(|r| reg_pair(d, r, ac, sc, i));

    if str_is(d, node.device_type, b"memory") {
        if let Some(reg) = node.reg {
            let mut i = 0;
            while let Some(r) = reg_pair(d, reg, ac, sc, i) {
                if r.size != 0 {
                    out.mem_seen += 1;
                    if out.nmem < MAX_MEM { out.mem[out.nmem] = r; out.nmem += 1; }
                }
                i += 1;
            }
        }
    }
    if str_is(d, node.device_type, b"cpu") {
        if let Some(reg) = node.reg {
            // A cpu's `reg` has the parent's address cells and NO size (cpus: #size-cells = 0).
            if let Some(id) = cells(d, reg.off, ac.clamp(1, 2)) {
                out.cpu_seen += 1;
                if out.ncpu < MAX_CPU { out.cpus[out.ncpu] = id; out.ncpu += 1; }
            }
        }
    }
    if compat_has(d, node.compatible, b"arm,pl011") {
        if let Some(r) = reg0(0) {
            if first_pl011.is_none() { *first_pl011 = Some(r); }
            if let Some(sp) = stdout_path {
                if path_names(d, sp, node.name) { *chosen_uart = Some(r); }
            }
        }
    }
    if compat_has(d, node.compatible, b"arm,pl031") && out.rtc.is_none() {
        out.rtc = reg0(0);
    }
    if compat_has(d, node.compatible, b"arm,gic-v3") {
        out.gicd = reg0(0);
        out.gicr = reg0(1);
    }
    if compat_has(d, node.compatible, b"arm,gic-v3-its") {
        out.its = reg0(0);
    }
    if compat_has(d, node.compatible, b"arm,armv8-timer") {
        if let Some(ints) = node.interrupts {
            // Triples (type, number, flags); type 1 = a PPI, whose INTID is 16 + the number.
            for k in 0..4 {
                let off = ints.off + 12 * k;
                if off + 12 > ints.off + ints.len { break; }
                if let (Some(1), Some(n)) = (be32(d, off), be32(d, off + 4)) {
                    out.timer_ppi[k] = 16 + n;
                }
            }
        }
    }
    if compat_has(d, node.compatible, b"arm,psci-1.0") || compat_has(d, node.compatible, b"arm,psci-0.2")
        || compat_has(d, node.compatible, b"arm,psci") {
        if str_is(d, node.method, b"hvc") { out.psci = Some(Psci::Hvc); }
        else if str_is(d, node.method, b"smc") { out.psci = Some(Psci::Smc); }
    }
    if compat_has(d, node.compatible, b"virtio,mmio") {
        if let (Some(r), Some(ints)) = (reg0(0), node.interrupts) {
            // (type 0 = an SPI, number, flags): the INTID is 32 + the number.
            if let (Some(0), Some(n)) = (be32(d, ints.off), be32(d, ints.off + 4)) {
                out.virtio_seen += 1;
                if out.nvirtio < MAX_VIRTIO { out.virtio[out.nvirtio] = (r.base, 32 + n); out.nvirtio += 1; }
            }
        }
    }
    if compat_has(d, node.compatible, b"pci-host-ecam-generic") {
        out.ecam = reg0(0);
        // `ranges`: (the child's address: 3 cells — the space code in bits 25:24 of the first,
        // then the 64-bit bus address), (the parent's address: the PARENT's address cells),
        // (the size: 2 cells). The first 32-bit memory window (code 2) is where a kernel with
        // no firmware before it assigns the BARs.
        if let Some(rg) = node.ranges {
            let entry = (3 + ac as usize + 2) * 4;
            let mut o = 0;
            while o + entry <= rg.len {
                let at = rg.off + o;
                let code = be32(d, at).map(|f| (f >> 24) & 3);
                let cpu = cells(d, at + 12, ac);
                let size = cells(d, at + 12 + 4 * ac as usize, 2);
                if let (Some(2), Some(base), Some(size)) = (code, cpu, size) {
                    if out.pci_mem32.is_none() && size != 0 { out.pci_mem32 = Some(Range { base, size }); }
                }
                o += entry;
            }
        }
    }
}

/// The total size the header claims — to find the end of a tree given only its start
/// (the loader passes a pointer, not a length). `None` if the bytes are not a tree.
pub fn total_size(head: &[u8]) -> Option<usize> {
    if be32(head, 0)? != MAGIC { return None; }
    Some(be32(head, 4)? as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QEMU 8.2.2, `-M virt,gic-version=3,its=on -cpu max -smp 2 -m 1024`, dumped with
    /// `dumpdtb=` and trimmed to its real size (QEMU pads the blob to 1 MiB).
    const VIRT: &[u8] = include_bytes!("../../../../kernel-unit/fixtures/qemu-virt-8.2.2-gicv3-its-smp2-m1024.dtb");

    #[test]
    fn the_stand_tree_yields_every_fact_the_boot_needs() {
        let s = parse(VIRT).unwrap();
        assert_eq!(s.nmem, 1);
        assert_eq!(s.mem[0], Range { base: 0x4000_0000, size: 0x4000_0000 });
        assert_eq!(s.uart, Some(Range { base: 0x0900_0000, size: 0x1000 }));
        assert_eq!(s.rtc, Some(Range { base: 0x0901_0000, size: 0x1000 }));
        assert_eq!(s.pci_mem32, Some(Range { base: 0x1000_0000, size: 0x2eff_0000 }));
        assert_eq!(s.gicd, Some(Range { base: 0x0800_0000, size: 0x1_0000 }));
        assert_eq!(s.gicr, Some(Range { base: 0x080a_0000, size: 0xf6_0000 }));
        assert_eq!(s.its, Some(Range { base: 0x0808_0000, size: 0x2_0000 }));
        assert_eq!(s.timer_ppi, [29, 30, 27, 26]);
        assert_eq!(s.psci, Some(Psci::Hvc));
        assert_eq!(s.ncpu, 2);
        assert_eq!(&s.cpus[..2], &[0, 1]);
        assert_eq!(s.nvirtio, 32);
        assert_eq!(s.virtio[0], (0x0a00_0000, 32 + 16));
        assert_eq!(s.virtio[31], (0x0a00_3e00, 32 + 16 + 31));
        assert_eq!(s.ecam, Some(Range { base: 0x40_1000_0000, size: 0x1000_0000 }));
        // no -append was given: there is no command line, and that is not an error
        assert_eq!(s.bootargs, None);
    }

    #[test]
    fn total_size_reads_the_header_and_refuses_a_stranger() {
        assert_eq!(total_size(VIRT), Some(VIRT.len()));
        assert_eq!(total_size(&[0u8; 8]), None);
        assert_eq!(total_size(&VIRT[..3]), None);
    }

    #[test]
    fn a_wrong_magic_is_not_a_tree() {
        let mut d = VIRT.to_vec();
        d[0] ^= 0xFF;
        assert_eq!(parse(&d), Err(Error::BadMagic));
        assert_eq!(parse(&[]), Err(Error::Truncated));
    }

    #[test]
    fn a_truncated_tree_is_an_error_at_every_cut() {
        // Every prefix of the real tree, from the header on, must fail cleanly rather
        // than read past the end — the loop is the point: no cut may panic.
        for cut in 0..VIRT.len() {
            match parse(&VIRT[..cut]) {
                Err(Error::Truncated) | Err(Error::BadStructure) => {}
                other => panic!("cut at {} gave {:?}", cut, other.map(|s| s.nmem)),
            }
        }
    }

    /// A tree built by hand, token by token: the cases the QEMU dump does not cover.
    struct Builder { st: Vec<u8>, strs: Vec<u8> }
    impl Builder {
        fn new() -> Self { Builder { st: Vec::new(), strs: Vec::new() } }
        fn w32(&mut self, v: u32) { self.st.extend_from_slice(&v.to_be_bytes()); }
        fn pad(&mut self) { while !self.st.len().is_multiple_of(4) { self.st.push(0); } }
        fn begin(&mut self, name: &str) { self.w32(BEGIN_NODE); self.st.extend_from_slice(name.as_bytes()); self.st.push(0); self.pad(); }
        fn end(&mut self) { self.w32(END_NODE); }
        fn nop(&mut self) { self.w32(NOP); }
        fn name_off(&mut self, name: &str) -> u32 {
            let off = self.strs.len() as u32;
            self.strs.extend_from_slice(name.as_bytes()); self.strs.push(0);
            off
        }
        fn prop(&mut self, name: &str, val: &[u8]) {
            let no = self.name_off(name);
            self.w32(PROP); self.w32(val.len() as u32); self.w32(no);
            self.st.extend_from_slice(val); self.pad();
        }
        fn prop_cells(&mut self, name: &str, cells: &[u32]) {
            let mut v = Vec::new();
            for c in cells { v.extend_from_slice(&c.to_be_bytes()); }
            self.prop(name, &v);
        }
        fn prop_str(&mut self, name: &str, s: &str) { let mut v = s.as_bytes().to_vec(); v.push(0); self.prop(name, &v); }
        fn finish(mut self, with_end: bool) -> Vec<u8> {
            if with_end { self.w32(END); }
            let off_struct = 40usize;
            let off_strings = off_struct + self.st.len();
            let total = off_strings + self.strs.len();
            let mut d = Vec::new();
            for v in [MAGIC, total as u32, off_struct as u32, off_strings as u32, 0, 17, 16, 0, self.strs.len() as u32, self.st.len() as u32] {
                d.extend_from_slice(&v.to_be_bytes());
            }
            d.extend_from_slice(&self.st);
            d.extend_from_slice(&self.strs);
            d
        }
    }

    fn tree_with(f: impl FnOnce(&mut Builder)) -> Vec<u8> {
        let mut b = Builder::new();
        b.begin("");
        b.prop_cells("#address-cells", &[2]);
        b.prop_cells("#size-cells", &[2]);
        f(&mut b);
        b.end();
        b.finish(true)
    }

    #[test]
    fn bootargs_stdout_path_and_a_second_uart() {
        let d = tree_with(|b| {
            b.begin("chosen");
            b.prop_str("bootargs", "console=ttyAMA0 talaria.mode=cell");
            b.prop_str("stdout-path", "/uart@8000000:115200");
            b.end();
            b.begin("pl011@9000000");
            b.prop_cells("reg", &[0, 0x0900_0000, 0, 0x1000]);
            b.prop("compatible", b"arm,pl011\0arm,primecell\0");
            b.end();
            b.begin("uart@8000000");
            b.prop_cells("reg", &[0, 0x0800_0000, 0, 0x1000]);
            b.prop("compatible", b"arm,pl011\0");
            b.end();
        });
        let s = parse(&d).unwrap();
        let ba = s.bootargs.unwrap();
        assert_eq!(&d[ba.off..ba.off + ba.len], b"console=ttyAMA0 talaria.mode=cell");
        // stdout-path wins over document order, and its `:baud` suffix is ignored
        assert_eq!(s.uart.unwrap().base, 0x0800_0000);
    }

    #[test]
    fn without_stdout_path_the_first_pl011_is_the_console_and_smc_is_read() {
        let d = tree_with(|b| {
            b.begin("serial@1000");
            b.prop_cells("reg", &[0, 0x1000, 0, 0x100]);
            b.prop("compatible", b"arm,pl011\0");
            b.end();
            b.begin("psci");
            b.prop_str("method", "smc");
            b.prop("compatible", b"arm,psci-0.2\0");
            b.end();
        });
        let s = parse(&d).unwrap();
        assert_eq!(s.uart, Some(Range { base: 0x1000, size: 0x100 }));
        assert_eq!(s.psci, Some(Psci::Smc));
    }

    #[test]
    fn cells_are_the_parents_and_a_bus_may_change_them() {
        // A 1/1-cell bus under a 2/2 root: the child's reg is two words, not four.
        let d = tree_with(|b| {
            b.begin("memory@80000000");
            b.prop_str("device_type", "memory");
            b.prop_cells("reg", &[0, 0x8000_0000, 1, 0, 0x10, 0, 0, 0x2000_0000]); // two ranges, the 2nd 4 GiB base
            b.end();
            b.begin("bus");
            b.prop_cells("#address-cells", &[1]);
            b.prop_cells("#size-cells", &[1]);
            b.begin("virtio_mmio@a000000");
            b.prop_cells("reg", &[0x0a00_0000, 0x200]);
            b.prop_cells("interrupts", &[0, 16, 1]);
            b.prop("compatible", b"virtio,mmio\0");
            b.end();
            b.end();
            b.begin("cpus");
            b.prop_cells("#address-cells", &[2]);
            b.prop_cells("#size-cells", &[0]);
            b.begin("cpu@100");
            b.prop_str("device_type", "cpu");
            b.prop_cells("reg", &[0, 0x100]);
            b.end();
            b.end();
        });
        let s = parse(&d).unwrap();
        assert_eq!(s.nmem, 2);
        assert_eq!(s.mem[0], Range { base: 0x8000_0000, size: 1 << 32 });
        assert_eq!(s.mem[1], Range { base: 0x10_0000_0000, size: 0x2000_0000 });
        assert_eq!(s.nvirtio, 1);
        assert_eq!(s.virtio[0], (0x0a00_0000, 48));
        assert_eq!(s.ncpu, 1);
        assert_eq!(s.cpus[0], 0x100);
    }

    #[test]
    fn more_than_the_tables_hold_is_counted_not_lost_silently() {
        let d = tree_with(|b| {
            for i in 0..(MAX_MEM + 2) as u32 {
                b.begin("memory");
                b.prop_str("device_type", "memory");
                b.prop_cells("reg", &[0, 0x1000_0000 * (i + 1), 0, 0x1000]);
                b.end();
            }
            b.begin("memory@0");
            b.prop_str("device_type", "memory");
            b.prop_cells("reg", &[0, 0, 0, 0]); // a zero-sized range is not a range
            b.end();
        });
        let s = parse(&d).unwrap();
        assert_eq!(s.nmem, MAX_MEM);
        assert_eq!(s.mem_seen, MAX_MEM + 2);
    }

    #[test]
    fn a_timer_without_ppis_and_partial_interrupt_lists() {
        let d = tree_with(|b| {
            b.begin("timer");
            b.prop_cells("interrupts", &[1, 13, 4, 0, 5, 4]); // one PPI, then an SPI — the SPI slot stays 0
            b.prop("compatible", b"arm,armv8-timer\0");
            b.end();
            b.begin("virtio_mmio@a000000");
            b.prop_cells("reg", &[0, 0x0a00_0000, 0, 0x200]);
            b.prop_cells("interrupts", &[1, 16, 1]); // a PPI on a virtio window is nonsense: skipped
            b.prop("compatible", b"virtio,mmio\0");
            b.end();
            b.begin("intc");
            b.prop("compatible", b"arm,gic-v3\0");
            b.prop_cells("reg", &[0, 0x0800_0000, 0, 0x1_0000]); // a distributor without a redistributor
            b.end();
        });
        let s = parse(&d).unwrap();
        assert_eq!(s.timer_ppi, [29, 0, 0, 0]);
        assert_eq!(s.nvirtio, 0);
        assert_eq!(s.gicd, Some(Range { base: 0x0800_0000, size: 0x1_0000 }));
        assert_eq!(s.gicr, None);
    }

    #[test]
    fn structure_errors_are_named() {
        // a property outside any node
        let mut b = Builder::new();
        b.prop_cells("x", &[1]);
        assert_eq!(parse(&b.finish(true)), Err(Error::BadStructure));
        // a node that never ends
        let mut b = Builder::new();
        b.begin("");
        b.begin("child");
        b.end();
        assert_eq!(parse(&b.finish(true)), Err(Error::BadStructure));
        // an unknown token
        let mut b = Builder::new();
        b.begin("");
        b.end();
        b.w32(7);
        assert_eq!(parse(&b.finish(true)), Err(Error::BadStructure));
        // an END_NODE with nothing open
        let mut b = Builder::new();
        b.end();
        assert_eq!(parse(&b.finish(true)), Err(Error::BadStructure));
        // nodes nested past the depth we walk
        let mut b = Builder::new();
        for i in 0..(MAX_DEPTH + 1) { b.begin(if i == 0 { "" } else { "n" }); }
        for _ in 0..(MAX_DEPTH + 1) { b.end(); }
        assert_eq!(parse(&b.finish(true)), Err(Error::BadStructure));
        // NOPs are skipped, a missing END is truncation
        let mut b = Builder::new();
        b.begin("");
        b.nop();
        b.end();
        assert!(parse(&b.finish(true)).is_ok());
        let mut b = Builder::new();
        b.begin("");
        b.end();
        assert_eq!(parse(&b.finish(false)), Err(Error::Truncated));
    }

    #[test]
    fn a_header_that_points_past_the_bytes_is_truncation() {
        let mut d = tree_with(|_| {});
        // the strings block claims to extend beyond the end
        let big = (d.len() as u32 + 100).to_be_bytes();
        d[32..36].copy_from_slice(&big);
        assert_eq!(parse(&d), Err(Error::Truncated));
        let mut d = tree_with(|_| {});
        let claim = (d.len() as u32 + 1).to_be_bytes(); // totalsize > the bytes given
        d[4..8].copy_from_slice(&claim);
        assert_eq!(parse(&d), Err(Error::Truncated));
    }
}
