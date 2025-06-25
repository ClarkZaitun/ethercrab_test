use super::storage::PduStorageRef;
use crate::ethernet::{EthernetAddress, EthernetFrame};
use crate::{
    ETHERCAT_ETHERTYPE, MAINDEVICE_ADDR,
    error::{Error, PduError},
    fmt,
    pdu_loop::frame_header::EthercatFrameHeader,
};
use core::sync::atomic::Ordering;
use ethercrab_wire::{EtherCrabWireRead, EtherCrabWireSized};

//返回帧的状态，大状态机套小状态机
/// What happened to a received Ethernet frame.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum ReceiveAction {
    /// The frame was ignored.
    ///
    /// This can be caused by other, non-EtherCAT traffic on the chosen network interface, e.g. if
    /// sending EtherCAT packets through a switch.
    Ignored,

    /// The frame was successfully processed as an EtherCAT packet.
    Processed,
}

/// EtherCAT frame receive adapter.
pub struct PduRx<'sto> {
    storage: PduStorageRef<'sto>,
    source_mac: EthernetAddress,
}

impl<'sto> PduRx<'sto> {
    pub(in crate::pdu_loop) fn new(storage: PduStorageRef<'sto>) -> Self {
        Self {
            storage,
            source_mac: MAINDEVICE_ADDR,
        }
    }

    /// Set the source MAC address to the given value.
    ///
    /// This is required on macOS (and BSD I believe) as the interface's MAC address cannot be
    /// overridden at the packet level for some reason.
    #[cfg(all(not(target_os = "linux"), unix))]
    pub(crate) fn set_source_mac(&mut self, new: EthernetAddress) {
        self.source_mac = new
    }

    //解析以太网 II 帧的 EtherCAT 协议数据单元（PDU），切换帧状态，唤醒帧任务
    /// Given a complete Ethernet II frame, parse a response PDU from it and wake the future that
    /// sent the frame.
    // NOTE: &mut self so this struct can only be used in one place.
    pub fn receive_frame(&mut self, ethernet_frame: &[u8]) -> Result<ReceiveAction, Error> {
        if self.should_exit() {
            return Ok(ReceiveAction::Ignored);
        }

        // 为什么需要套一次EthernetFrame？
        // rust强制类型转换该怎么写？
        let raw_packet = EthernetFrame::new_checked(ethernet_frame)?;

        // Look for EtherCAT packets whilst ignoring broadcast packets sent from self. As per
        // <https://github.com/OpenEtherCATsociety/SOEM/issues/585#issuecomment-1013688786>, the
        // first SubDevice will set the second bit of the MSB of the MAC address (U/L bit). This means
        // if we send e.g. 10:10:10:10:10:10, we receive 12:10:10:10:10:10 which passes through this
        // filter.
        // 检查EtherCAT帧类型和源MAC
        if raw_packet.ethertype() != ETHERCAT_ETHERTYPE || raw_packet.src_addr() == self.source_mac
        {
            fmt::trace!("Ignore frame");

            return Ok(ReceiveAction::Ignored);
        }

        //返回指向有效载荷（数据区，即EtherCAT帧）的指针，而不检查 802.1Q。
        let i = raw_packet.payload();

        //inspect_err：这是 Result 类型的方法，它接收一个闭包作为参数。当 Result 为 Err 时，会调用这个闭包，且不会改变 Result 的值
        let frame_header = EthercatFrameHeader::unpack_from_slice(i).inspect_err(|&e| {
            fmt::error!("Failed to parse frame header: {}", e);
        })?;

        // 不可能发生吧，以太网帧要求有60字节
        if frame_header.payload_len == 0 {
            fmt::trace!("Ignoring empty frame");

            return Ok(ReceiveAction::Ignored);
        }

        // 获得EtherCAT帧数据区
        // Skip EtherCAT header and get PDU(s) payload
        let i = i
            .get(
                EthercatFrameHeader::PACKED_LEN//2
                    ..(EthercatFrameHeader::PACKED_LEN + usize::from(frame_header.payload_len)), //2+EtherCAT帧长度
            )
            .ok_or_else(|| {
                fmt::error!("Received frame is too short");

                Error::ReceiveFrame
            })?;

        // `i` now contains the EtherCAT frame payload, consisting of one or more PDUs including
        // their headers and payloads.

        // Second byte of first PDU header is the index
        // 获取第一个EtherCAT数据报的 index
        let pdu_idx = *i.get(1).ok_or(Error::Internal)?;

        //
        // 需要重构：假设返回帧中的所有 PDU 都有相同的帧索引，因此我们可以使用第一个。
        // We're assuming all PDUs in the returned frame have the same frame index, so we can just
        // use the first one.

        // 通过index找到之前发送的帧在缓冲区中的帧索引
        // PDU has its own EtherCAT index. This needs mapping back to the original frame.
        let frame_index = self
            .storage
            .frame_index_by_first_pdu_index(pdu_idx)
            .ok_or(Error::Pdu(PduError::Decode))?;

        fmt::trace!(
            "Receiving frame index {} (found from PDU {:#04x})",
            frame_index,
            pdu_idx
        );

        // Sent 状态的帧标记为 RxBusy 状态，返回帧缓冲区的帧
        let mut frame = self
            .storage
            .claim_receiving(frame_index)
            .ok_or(PduError::InvalidIndex(frame_index))?;

        //获取一个可变的字节切片&mut [u8]，该切片指向 EtherCAT数据报
        let frame_data = frame.buf_mut();

        frame_data
            .get_mut(0..i.len())
            .ok_or(Error::Internal)?
            .copy_from_slice(i); //i 包含了接收到的 EtherCAT数据报，将i中的数据复制到frame_data（帧缓冲区的帧）中

        // 标记 RxBusy 帧的状态为RxDone，唤醒帧相关的任务
        frame.mark_received()?;

        Ok(ReceiveAction::Processed)
    }

    /// Returns `true` if the PDU sender should exit.
    ///
    /// This will be triggered by [`MainDevice::release_all`](crate::MainDevice::release_all).
    pub fn should_exit(&self) -> bool {
        self.storage.exit_flag.load(Ordering::Acquire)
    }

    /// Reset this object ready for reuse.
    ///
    /// When giving back ownership of the `PduRx`, be sure to call
    /// [`release`](crate::PduRx::release) to ensure all internal state is correct before reuse.
    pub fn release(self) -> Self {
        self.storage.exit_flag.store(false, Ordering::Relaxed);

        self
    }
}
