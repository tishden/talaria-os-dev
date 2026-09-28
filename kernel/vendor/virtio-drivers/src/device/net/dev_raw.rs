use super::{Config, EthernetAddress, Features, VirtioNetHdr};
use super::{MIN_BUFFER_LEN, NET_HDR_SIZE, QUEUE_RECEIVE, QUEUE_TRANSMIT, SUPPORTED_FEATURES};
use crate::config::read_config;
use crate::hal::Hal;
use crate::queue::VirtQueue;
use crate::transport::Transport;
use crate::{Error, Result};
use log::{debug, info, warn};
use zerocopy::IntoBytes;

/// Raw driver for a VirtIO network device.
///
/// This is a raw version of the VirtIONet driver. It provides non-blocking
/// methods for transmitting and receiving raw slices, without the buffer
/// management. For more higher-level functions such as receive buffer backing,
/// see [`VirtIONet`].
///
/// [`VirtIONet`]: super::VirtIONet
pub struct VirtIONetRaw<H: Hal, T: Transport, const QUEUE_SIZE: usize> {
    transport: T,
    mac: EthernetAddress,
    recv_queue: VirtQueue<H, QUEUE_SIZE>,
    send_queue: VirtQueue<H, QUEUE_SIZE>,
}

impl<H: Hal, T: Transport, const QUEUE_SIZE: usize> VirtIONetRaw<H, T, QUEUE_SIZE> {
    /// Create a new VirtIO-Net driver.
    pub fn new(mut transport: T) -> Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);
        info!("negotiated_features {:?}", negotiated_features);

        // Read configuration space.
        let mac = transport.read_consistent(|| read_config!(transport, Config, mac))?;
        let status = read_config!(transport, Config, status)?;
        debug!("Got MAC={:02x?}, status={:?}", mac, status);

        let send_queue = VirtQueue::new(
            &mut transport,
            QUEUE_TRANSMIT,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
        )?;
        let recv_queue = VirtQueue::new(
            &mut transport,
            QUEUE_RECEIVE,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
        )?;

        transport.finish_init();

        Ok(VirtIONetRaw {
            transport,
            mac,
            recv_queue,
            send_queue,
        })
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.transport.ack_interrupt()
    }

    /// Disable interrupts.
    pub fn disable_interrupts(&mut self) {
        self.send_queue.set_dev_notify(false);
        self.recv_queue.set_dev_notify(false);
    }

    /// Enable interrupts.
    pub fn enable_interrupts(&mut self) {
        self.send_queue.set_dev_notify(true);
        self.recv_queue.set_dev_notify(true);
    }

    /// Talaria: an interrupt on the RECEIVE ring ONLY.
    ///
    /// We never wait for transmit completions: the buffers come back on the next
    /// pass over the stack, so an interrupt for them is pure waste — and not a
    /// small one. A measurement under mongodb gave 625 616 interrupts for 358 297
    /// received frames, i.e. 1.7 interrupts per frame, and the second half of them
    /// was exactly the transmit side.
    pub fn enable_recv_interrupts(&mut self) {
        self.send_queue.set_dev_notify(false);
        self.recv_queue.set_dev_notify(true);
    }

    /// Talaria: the RECEIVE ring's word to sleep on while idle (talaria.netidle=1).
    pub fn rx_used_watch(&self) -> (*const u32, u32) {
        self.recv_queue.used_watch()
    }


    /// Get MAC address.
    pub fn mac_address(&self) -> EthernetAddress {
        self.mac
    }

    /// Whether can send packet.
    pub fn can_send(&self) -> bool {
        self.send_queue.available_desc() >= 2
    }

    /// Whether the length of the receive buffer is valid.
    fn check_rx_buf_len(rx_buf: &[u8]) -> Result<()> {
        if rx_buf.len() < MIN_BUFFER_LEN {
            warn!("Receive buffer len {} is too small", rx_buf.len());
            Err(Error::InvalidParam)
        } else {
            Ok(())
        }
    }

    /// Whether the length of the transmit buffer is valid.
    fn check_tx_buf_len(tx_buf: &[u8]) -> Result<()> {
        if tx_buf.len() < NET_HDR_SIZE {
            warn!("Transmit buffer len {} is too small", tx_buf.len());
            Err(Error::InvalidParam)
        } else {
            Ok(())
        }
    }

    /// Fill the header of the `buffer` with [`VirtioNetHdr`].
    ///
    /// If the `buffer` is not large enough, it returns [`Error::InvalidParam`].
    pub fn fill_buffer_header(&self, buffer: &mut [u8]) -> Result<usize> {
        if buffer.len() < NET_HDR_SIZE {
            return Err(Error::InvalidParam);
        }
        let header = VirtioNetHdr::default();
        buffer[..NET_HDR_SIZE].copy_from_slice(header.as_bytes());
        Ok(NET_HDR_SIZE)
    }

    /// Submits a request to transmit a buffer immediately without waiting for
    /// the transmission to complete.
    ///
    /// It will submit request to the VirtIO net device and return a token
    /// identifying the position of the first descriptor in the chain. If there
    /// are not enough descriptors to allocate, then it returns
    /// [`Error::QueueFull`].
    ///
    /// The caller needs to fill the `tx_buf` with a header by calling
    /// [`fill_buffer_header`] before transmission. Then it calls [`poll_transmit`]
    /// with the returned token to check whether the device has finished handling
    /// the request. Once it has, the caller must call [`transmit_complete`] with
    /// the same buffer before reading the result (transmitted length).
    ///
    /// # Safety
    ///
    /// `tx_buf` is still borrowed by the underlying VirtIO net device even after
    /// this method returns. Thus, it is the caller's responsibility to guarantee
    /// that they are not accessed before the request is completed in order to
    /// avoid data races.
    ///
    /// [`fill_buffer_header`]: Self::fill_buffer_header
    /// [`poll_transmit`]: Self::poll_transmit
    /// [`transmit_complete`]: Self::transmit_complete
    pub unsafe fn transmit_begin(&mut self, tx_buf: &[u8]) -> Result<u16> {
        Self::check_tx_buf_len(tx_buf)?;
        let token = self.send_queue.add(&[tx_buf], &mut [])?;
        if self.send_queue.should_notify() {
            self.transport.notify(QUEUE_TRANSMIT);
        }
        Ok(token)
    }

    /// Talaria (batched notify): as `transmit_begin`, but WITHOUT the notify — the
    /// caller batches frames and notifies the device once through
    /// [`transmit_notify`]. That saves an MMIO kick (a VMEXIT) per frame.
    ///
    /// # Safety
    /// The same requirements as [`transmit_begin`].
    pub unsafe fn transmit_begin_no_notify(&mut self, tx_buf: &[u8]) -> Result<u16> {
        Self::check_tx_buf_len(tx_buf)?;
        self.send_queue.add(&[tx_buf], &mut [])
    }

    /// Talaria: notify the device of a filled TX queue (once per batch).
    /// Returns true if a notify was really sent (the device is not polling).
    pub fn transmit_notify(&mut self) -> bool {
        // A wrap-safe vring_need_event over the whole batch, with the mandatory mb() inside.
        if self.send_queue.prepare_notify_batched() {
            self.transport.notify(QUEUE_TRANSMIT);
            true
        } else {
            false
        }
    }

    /// Fetches the token of the next completed transmission request from the
    /// used ring and returns it, without removing it from the used ring. If
    /// there are no pending completed requests it returns [`None`].
    pub fn poll_transmit(&mut self) -> Option<u16> {
        self.send_queue.peek_used()
    }

    /// Completes a transmission operation which was started by [`transmit_begin`].
    /// Returns number of bytes transmitted.
    ///
    /// # Safety
    ///
    /// The same buffer must be passed in again as was passed to
    /// [`transmit_begin`] when it returned the token.
    ///
    /// [`transmit_begin`]: Self::transmit_begin
    pub unsafe fn transmit_complete(&mut self, token: u16, tx_buf: &[u8]) -> Result<usize> {
        let len = self.send_queue.pop_used(token, &[tx_buf], &mut [])?;
        Ok(len as usize)
    }

    /// Submits a request to receive a buffer immediately without waiting for
    /// the reception to complete.
    ///
    /// It will submit request to the VirtIO net device and return a token
    /// identifying the position of the first descriptor in the chain. If there
    /// are not enough descriptors to allocate, then it returns
    /// [`Error::QueueFull`].
    ///
    /// The caller can then call [`poll_receive`] with the returned token to
    /// check whether the device has finished handling the request. Once it has,
    /// the caller must call [`receive_complete`] with the same buffer before
    /// reading the response.
    ///
    /// # Safety
    ///
    /// `rx_buf` is still borrowed by the underlying VirtIO net device even after
    /// this method returns. Thus, it is the caller's responsibility to guarantee
    /// that they are not accessed before the request is completed in order to
    /// avoid data races.
    ///
    /// [`poll_receive`]: Self::poll_receive
    /// [`receive_complete`]: Self::receive_complete
    pub unsafe fn receive_begin(&mut self, rx_buf: &mut [u8]) -> Result<u16> {
        Self::check_rx_buf_len(rx_buf)?;
        let token = self.recv_queue.add(&[], &mut [rx_buf])?;
        if self.recv_queue.should_notify() {
            self.transport.notify(QUEUE_RECEIVE);
        }
        Ok(token)
    }

    /// Talaria: re-post an RX buffer without a notify (a batch plus [`receive_notify`]).
    ///
    /// # Safety
    /// The same requirements as [`receive_begin`].
    pub unsafe fn receive_begin_no_notify(&mut self, rx_buf: &mut [u8]) -> Result<u16> {
        Self::check_rx_buf_len(rx_buf)?;
        self.recv_queue.add(&[], &mut [rx_buf])
    }

    /// Talaria: notify about the RX buffers (once per batch).
    pub fn receive_notify(&mut self) -> bool {
        if self.recv_queue.prepare_notify_batched() {
            self.transport.notify(QUEUE_RECEIVE);
            true
        } else {
            false
        }
    }

    /// Fetches the token of the next completed reception request from the
    /// used ring and returns it, without removing it from the used ring. If
    /// there are no pending completed requests it returns [`None`].
    pub fn poll_receive(&self) -> Option<u16> {
        self.recv_queue.peek_used()
    }

    /// Completes a transmission operation which was started by [`receive_begin`].
    ///
    /// After completion, the `rx_buf` will contain a header followed by the
    /// received packet. It returns the length of the header and the length of
    /// the packet.
    ///
    /// # Safety
    ///
    /// The same buffer must be passed in again as was passed to
    /// [`receive_begin`] when it returned the token.
    ///
    /// [`receive_begin`]: Self::receive_begin
    pub unsafe fn receive_complete(
        &mut self,
        token: u16,
        rx_buf: &mut [u8],
    ) -> Result<(usize, usize)> {
        let len = self.recv_queue.pop_used(token, &[], &mut [rx_buf])? as usize;
        let packet_len = len.checked_sub(NET_HDR_SIZE).ok_or(Error::IoError)?;
        Ok((NET_HDR_SIZE, packet_len))
    }

    /// Sends a packet to the network, and blocks until the request completed.
    pub fn send(&mut self, tx_buf: &[u8]) -> Result {
        let header = VirtioNetHdr::default();
        if tx_buf.is_empty() {
            // Special case sending an empty packet, to avoid adding an empty buffer to the
            // virtqueue.
            self.send_queue.add_notify_wait_pop(
                &[header.as_bytes()],
                &mut [],
                &mut self.transport,
            )?;
        } else {
            self.send_queue.add_notify_wait_pop(
                &[header.as_bytes(), tx_buf],
                &mut [],
                &mut self.transport,
            )?;
        }
        Ok(())
    }

    /// Blocks and waits for a packet to be received.
    ///
    /// After completion, the `rx_buf` will contain a header followed by the
    /// received packet. It returns the length of the header and the length of
    /// the packet.
    pub fn receive_wait(&mut self, rx_buf: &mut [u8]) -> Result<(usize, usize)> {
        // SAFETY: After calling `receive_begin`, `rx_buf` is not accessed
        // until calling `receive_complete` when the request is complete.
        let token = unsafe { self.receive_begin(rx_buf)? };
        while self.poll_receive().is_none() {
            core::hint::spin_loop();
        }
        // SAFETY: This `rx_buf` is the same one passed to `receive_begin`.
        unsafe { self.receive_complete(token, rx_buf) }
    }
}

impl<H: Hal, T: Transport, const QUEUE_SIZE: usize> Drop for VirtIONetRaw<H, T, QUEUE_SIZE> {
    fn drop(&mut self) {
        // Clear any pointers pointing to DMA regions, so the device doesn't try to access them
        // after they have been freed.
        self.transport.queue_unset(QUEUE_RECEIVE);
        self.transport.queue_unset(QUEUE_TRANSMIT);
    }
}
