//! VFIO plumbing for accessing the device from userspace (noiommu mode).
//! Opens the container/group/device, maps BAR0, enables bus master.
//! In noiommu, DMA uses physical addresses (see dma.rs) — VFIO_IOMMU_MAP_DMA
//! is not used.

use libc::{c_void, ioctl};
use std::fs::{File, OpenOptions};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

// VFIO ioctl numbers (uapi/linux/vfio.h), type 0x3B (';').
const VFIO_GET_API_VERSION: u64 = 15204; // _IO(';', 100) = 0x3B64
const VFIO_CHECK_EXTENSION: u64 = 15205;
const VFIO_SET_IOMMU: u64 = 15206;
const VFIO_GROUP_GET_STATUS: u64 = 15207;
const VFIO_GROUP_SET_CONTAINER: u64 = 15208;
const VFIO_GROUP_GET_DEVICE_FD: u64 = 15210;
const VFIO_DEVICE_GET_REGION_INFO: u64 = 15212;

const VFIO_API_VERSION: i32 = 0;
const VFIO_NOIOMMU_IOMMU: i32 = 8;
const VFIO_GROUP_FLAGS_VIABLE: u32 = 1;

#[repr(C)]
struct VfioGroupStatus {
    argsz: u32,
    flags: u32,
}

#[repr(C)]
struct VfioRegionInfo {
    argsz: u32,
    flags: u32,
    index: u32,
    cap_offset: u32,
    size: u64,
    offset: u64,
}

// The PCI config-space region index in VFIO.
const VFIO_PCI_CONFIG_REGION_INDEX: u32 = 7;
const VFIO_PCI_BAR0_REGION_INDEX: u32 = 0;

pub struct VfioDevice {
    _container: File,
    _group: File,
    device: File,
    cfg_offset: u64,
}

pub struct Bar {
    pub ptr: *mut u8,
}

impl VfioDevice {
    pub fn open(pci_addr: &str, group: u32) -> std::io::Result<Self> {
        let container = OpenOptions::new().read(true).write(true).open("/dev/vfio/vfio")?;
        let cfd = container.as_raw_fd();
        unsafe {
            let ver = ioctl(cfd, VFIO_GET_API_VERSION);
            assert_eq!(ver, VFIO_API_VERSION, "VFIO API version");
            let ext = ioctl(cfd, VFIO_CHECK_EXTENSION, VFIO_NOIOMMU_IOMMU as u64);
            assert!(ext > 0, "noiommu extension unsupported");
        }

        let gpath = format!("/dev/vfio/noiommu-{}", group);
        let gfile = OpenOptions::new().read(true).write(true).open(&gpath)?;
        let gfd = gfile.as_raw_fd();

        let mut st = VfioGroupStatus { argsz: 8, flags: 0 };
        unsafe {
            assert_eq!(ioctl(gfd, VFIO_GROUP_GET_STATUS, &mut st), 0, "group status");
            assert!(st.flags & VFIO_GROUP_FLAGS_VIABLE != 0, "group not viable");
            assert_eq!(
                ioctl(gfd, VFIO_GROUP_SET_CONTAINER, &cfd as *const RawFd),
                0,
                "set container"
            );
            assert_eq!(
                ioctl(cfd, VFIO_SET_IOMMU, VFIO_NOIOMMU_IOMMU as u64),
                0,
                "set iommu noiommu"
            );
        }

        let name = std::ffi::CString::new(pci_addr).unwrap();
        let dfd = unsafe { ioctl(gfd, VFIO_GROUP_GET_DEVICE_FD, name.as_ptr()) };
        assert!(dfd >= 0, "get device fd");
        let device = unsafe { File::from_raw_fd(dfd) };

        // config-space offset (for bus master through config space)
        let mut ci = VfioRegionInfo {
            argsz: std::mem::size_of::<VfioRegionInfo>() as u32,
            flags: 0,
            index: VFIO_PCI_CONFIG_REGION_INDEX,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        unsafe {
            assert_eq!(ioctl(dfd, VFIO_DEVICE_GET_REGION_INFO, &mut ci), 0, "cfg region");
        }

        let dev = VfioDevice {
            _container: container,
            _group: gfile,
            device,
            cfg_offset: ci.offset,
        };
        dev.enable_bus_master();
        Ok(dev)
    }

    pub fn map_bar0(&self) -> std::io::Result<Bar> {
        let mut ri = VfioRegionInfo {
            argsz: std::mem::size_of::<VfioRegionInfo>() as u32,
            flags: 0,
            index: VFIO_PCI_BAR0_REGION_INDEX,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        let dfd = self.device.as_raw_fd();
        unsafe {
            assert_eq!(ioctl(dfd, VFIO_DEVICE_GET_REGION_INFO, &mut ri), 0, "bar0 region");
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ri.size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                dfd,
                ri.offset as i64,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap bar0");
        Ok(Bar { ptr: ptr as *mut u8 })
    }

    fn read_cfg(&self, off: u64, buf: &mut [u8]) {
        let n = unsafe {
            libc::pread(
                self.device.as_raw_fd(),
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                (self.cfg_offset + off) as i64,
            )
        };
        assert_eq!(n as usize, buf.len(), "cfg read");
    }

    fn write_cfg(&self, off: u64, buf: &[u8]) {
        let n = unsafe {
            libc::pwrite(
                self.device.as_raw_fd(),
                buf.as_ptr() as *const c_void,
                buf.len(),
                (self.cfg_offset + off) as i64,
            )
        };
        assert_eq!(n as usize, buf.len(), "cfg write");
    }

    /// COMMAND register (offset 0x04): set Bus Master Enable (bit 2).
    pub fn enable_bus_master(&self) {
        let mut cmd = [0u8; 2];
        self.read_cfg(0x04, &mut cmd);
        let v = u16::from_le_bytes(cmd) | (1 << 2) | (1 << 1); // BME + MEM
        self.write_cfg(0x04, &v.to_le_bytes());
    }

    pub fn read_ids(&self) -> (u16, u16) {
        let mut b = [0u8; 4];
        self.read_cfg(0x00, &mut b);
        (
            u16::from_le_bytes([b[0], b[1]]),
            u16::from_le_bytes([b[2], b[3]]),
        )
    }
}
