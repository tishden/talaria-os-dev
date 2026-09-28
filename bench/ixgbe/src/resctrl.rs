//! LLC partitioning through resctrl (Intel CAT). Creates a group, sets the L3
//! mask and puts the current thread into it. Needs root and a mounted resctrl.
use std::io::Write;

pub fn available() -> bool {
    std::path::Path::new("/sys/fs/resctrl/info/L3/cbm_mask").exists()
}

/// Configure group `name` with mask `mask` (0xff for example) and add the current tid.
pub fn setup(name: &str, mask: u64) -> std::io::Result<()> {
    let dir = format!("/sys/fs/resctrl/{name}");
    std::fs::create_dir_all(&dir)?;
    // L3 domains: apply the same mask to all of them (usually one socket = "L3:0=...")
    let cbm = std::fs::read_to_string("/sys/fs/resctrl/info/L3/cbm_mask")
        .unwrap_or_default();
    let _ = cbm;
    let schemata = format!("L3:0={:x}\n", mask);
    let mut sf = std::fs::OpenOptions::new().write(true).open(format!("{dir}/schemata"))?;
    sf.write_all(schemata.as_bytes())?;
    // put the current thread (tid) into the group
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    let mut tf = std::fs::OpenOptions::new().write(true).open(format!("{dir}/tasks"))?;
    tf.write_all(format!("{tid}\n").as_bytes())?;
    Ok(())
}
