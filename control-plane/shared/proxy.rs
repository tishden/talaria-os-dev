// The shared ABI of the syscall-proxy ring: the cell (producer/requester) queues requests,
// the control plane (consumer) executes syscalls on its behalf. include! into both.
pub const PROXY_MAGIC: u64 = 0x7072_6f78_795f_7401; // "proxy_t\1"
pub const LOG_SLOTS: usize = 128;
pub const LOG_LINE: usize = 200;

pub const OP_READFILE: u32 = 2;

#[repr(C)]
pub struct ProxyShm {
    pub magic: u64,
    // an asynchronous log ring (SPSC: the cell writes, the CP reads)
    pub log_head: u64,
    pub log_tail: u64,
    pub log: [[u8; LOG_LINE]; LOG_SLOTS], // [0]=len (u8), then utf8
    // a synchronous mailbox (one operation in flight)
    pub req_seq: u64,
    pub req_op: u32,
    pub req_len: u32,
    pub req_data: [u8; 256],
    pub rep_seq: u64,
    pub rep_len: u32,
    pub rep_data: [u8; 1024],
}
