//! An EtherCAT frame header.

use crate::LEN_MASK;
use ethercrab_wire::{EtherCrabWireRead, EtherCrabWireSized, EtherCrabWireWrite};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, ethercrab_wire::EtherCrabWireRead)]
#[repr(u8)] //指定枚举或结构体的底层表示形式
pub(crate) enum ProtocolType {
    DlPdu = 0x01u8, //therCAT设备通信
                    // Not currently supported.
                    // NetworkVariables = 0x04,//EAP过程数据通信
                    // Mailbox = 0x05,//EAP邮箱通信
                    // #[wire(catch_all)]
                    // Unknown(u8),
}

/// An EtherCAT frame header.
///
/// An EtherCAT frame can contain one or more PDUs after this header, each starting with a
/// [`PduHeader`](crate::pdu_loop::pdu_header).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
//实现 Hash trait 允许将该结构体实例用作 HashMap 或 HashSet 的键
pub struct EthercatFrameHeader {
    pub(crate) payload_len: u16,       //EtherCAT帧数据区长度
    pub(crate) protocol: ProtocolType, //EtherCAT帧协议
}

impl EtherCrabWireSized for EthercatFrameHeader {
    const PACKED_LEN: usize = 2;

    type Buffer = [u8; 2];

    fn buffer() -> Self::Buffer {
        [0u8; 2]
    }
}

impl EtherCrabWireRead for EthercatFrameHeader {
    //将 u16 的数据展开为一个 u16 和 u8，获得EtherCAT帧数据区长度
    fn unpack_from_slice(buf: &[u8]) -> Result<Self, ethercrab_wire::WireError> {
        //调用 u16 类型的 unpack_from_slice 方法，尝试从传入的字节切片 buf 中解析出一个 u16 类型的值
        let raw = u16::unpack_from_slice(buf)?; //ethercrab-wire对基本类型实现 unpack_from_slice，这里可能返回 WireError

        Ok(Self {
            payload_len: raw & LEN_MASK,
            protocol: ProtocolType::try_from((raw >> 12) as u8)?, //EtherCAT帧类型为4bit
        })
    }
}

impl EtherCrabWireWrite for EthercatFrameHeader {
    // 压缩数据长度和协议到2字节的EtherCAT帧头中，将EtherCAT帧头写入帧中
    fn pack_to_slice_unchecked<'buf>(&self, buf: &'buf mut [u8]) -> &'buf [u8] {
        // 压缩数据长度和协议到2字节的EtherCAT帧头中
        // Protocol in last 4 bits
        let raw = self.payload_len | (self.protocol as u16) << 12;

        raw.pack_to_slice_unchecked(buf)
    }

    fn packed_len(&self) -> usize {
        Self::PACKED_LEN //2
    }
}

impl EthercatFrameHeader {
    // 创建EtherCAT帧头
    /// Create a new PDU frame header.
    pub fn pdu(len: u16) -> Self {
        debug_assert!(
            len <= LEN_MASK, // 应该改为1498或1470
            "Frame length may not exceed {} bytes",
            LEN_MASK
        );

        Self {
            payload_len: len & LEN_MASK,
            // Only support DlPdu (for now?)
            protocol: ProtocolType::DlPdu,
        }
    }

    /// Convenience method for naming consistency.
    pub(crate) const fn header_len() -> usize {
        Self::PACKED_LEN //2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdu_header() {
        let header = EthercatFrameHeader::pdu(0x28);

        let mut buf = [0u8; 2];

        let packed = header.pack_to_slice_unchecked(&mut buf);

        let expected = &0b0001_0000_0010_1000u16.to_le_bytes();

        assert_eq!(packed, expected);
    }

    #[test]
    fn decode_pdu_len() {
        let raw = 0b0001_0000_0010_1000u16;

        let header = EthercatFrameHeader::unpack_from_slice(&raw.to_le_bytes()).unwrap();

        assert_eq!(header.payload_len, 0x28);
        assert_eq!(header.protocol, ProtocolType::DlPdu);
    }

    #[test]
    fn parse() {
        // Header from packet #39, soem-sdinfo-ek1100-only.pcapng
        let raw = [0x3cu8, 0x10];

        let header = EthercatFrameHeader::unpack_from_slice(&raw).unwrap();

        assert_eq!(header.payload_len, 0x3c);
        assert_eq!(header.protocol, ProtocolType::DlPdu);
    }
}
