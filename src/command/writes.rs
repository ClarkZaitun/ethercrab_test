use crate::{MainDevice, error::Error, pdu_loop::ReceivedPdu};
use ethercrab_wire::{EtherCrabWireRead, EtherCrabWireWrite};

/// Write commands.
#[derive(PartialEq, Eq, Debug, Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Writes {
    /// BWR.
    Bwr {
        /// Autoincremented by each SubDevice visited.
        address: u16,

        /// Memory location to write to.
        register: u16,
    },
    /// APWR.
    Apwr {
        /// Auto increment counter.
        address: u16,

        /// Memory location to write to.
        register: u16,
    },
    /// FPWR.
    Fpwr {
        /// Configured station address.
        address: u16,

        /// Memory location to read from.
        register: u16,
    },
    /// LWR.
    Lwr {
        /// Logical address.
        address: u32,
    },

    /// LRW.
    Lrw {
        /// Logical address.
        address: u32,
    },
}

/// A wrapped version of a [`Writes`] exposing a builder API used to send/receive data over the
/// wire.
#[derive(Debug, Copy, Clone)]
pub struct WrappedWrite {
    /// EtherCAT command.
    pub command: Writes,
    /// Expected working counter.
    wkc: Option<u16>,
    len_override: Option<u16>,
}

impl WrappedWrite {
    pub(crate) fn new(command: Writes) -> Self {
        Self {
            command,
            wkc: Some(1),
            len_override: None,
        }
    }

    // 没有给数据长度len_override时，后文CreatedFrame::push_pdu会根据data的数据长度自动生成
    /// Set an explicit length for the PDU instead of taking it from the sent data.
    ///
    /// The length will be the _maximum_ of the value set here and the data sent.
    pub fn with_len(self, new_len: impl Into<u16>) -> Self {
        Self {
            len_override: Some(new_len.into()),
            ..self
        }
    }

    // 不应该有这个函数
    // 如果WKC不是预期值，不返回错误
    /// Do not return an error if the working counter is different from the expected value.
    ///
    /// The default value is `1` and can be overridden with [`with_wkc`](WrappedWrite::with_wkc).
    pub fn ignore_wkc(self) -> Self {
        Self { wkc: None, ..self } //..self 表示保留原实例的所有其他字段值不变
    }

    /// Change the expected working counter from its default of `1`.
    // 设置预期WKC
    pub fn with_wkc(self, wkc: u16) -> Self {
        Self {
            wkc: Some(wkc),
            ..self
        }
    }

    // 没有给数据长度len_override，后文CreatedFrame::push_pdu会根据data的数据长度自动生成
    // 不检查WKC
    /// Send a payload with a length set by [`with_len`](WrappedWrite::with_len), ignoring the
    /// response.
    pub async fn send<'maindevice>(
        self,
        maindevice: &'maindevice MainDevice<'maindevice>,
        data: impl EtherCrabWireWrite,
    ) -> Result<(), Error> {
        self.common(maindevice, data, self.len_override).await?;

        Ok(())
    }

    // 可能会检查WKC。有预期值时检查WKC是否符合预期，否则直接返回WKC
    /// Send a value, returning the response returned from the network.
    pub async fn send_receive<'maindevice, T>(
        self,
        maindevice: &'maindevice MainDevice<'maindevice>,
        value: impl EtherCrabWireWrite,
    ) -> Result<T, Error>
    where
        T: EtherCrabWireRead,
    {
        self.common(maindevice, value, None)
            .await?
            .maybe_wkc(self.wkc)
            .and_then(|data| Ok(T::unpack_from_slice(&data)?))
    }

    // 可能会检查WKC。有预期值时检查WKC是否符合预期，否则直接返回WKC
    /// Similar to [`send_receive`](WrappedWrite::send_receive) but returns a slice.
    pub async fn send_receive_slice<'maindevice>(
        self,
        maindevice: &'maindevice MainDevice<'maindevice>,
        value: impl EtherCrabWireWrite,
    ) -> Result<ReceivedPdu<'maindevice>, Error> {
        self.common(maindevice, value, None)
            .await?
            .maybe_wkc(self.wkc)
    }

    // Some manual monomorphisation
    fn common<'maindevice>(
        &self,
        maindevice: &'maindevice MainDevice<'maindevice>,
        value: impl EtherCrabWireWrite,
        len_override: Option<u16>,
    ) -> impl core::future::Future<Output = Result<ReceivedPdu<'maindevice>, Error>> {
        maindevice.single_pdu(self.command.into(), value, len_override)
    }
}
