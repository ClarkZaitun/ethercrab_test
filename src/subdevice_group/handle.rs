use crate::{
    GroupId, MainDevice, SubDevice, SubDeviceGroup, SubDeviceRef, error::Error, fmt, pdi::PdiOffset,
};

/// A trait implemented only by [`SubDeviceGroup`] so multiple groups with different const params
/// can be stored in a hashmap, `Vec`, etc.
#[doc(hidden)] // 让此 trait 在生成的文档中隐藏，避免用户直接使用
#[sealed::sealed] // 通过在定义trait和impl的地方前面添加这个宏，实现密封。如果impl该trait时没加宏，编译会报错
pub trait SubDeviceGroupHandle: Sync {
    // : Sync：trait bound，表明实现 SubDeviceGroupHandle 的类型必须同时实现 Sync trait。Sync 用于标记类型可以安全地在多个线程间共享引用，保证线程安全
    /// Get the group's ID.
    // 不是原子变量
    // 获取组ID
    fn id(&self) -> GroupId;

    /// Add a SubDevice device to this group.
    // 添加一个从站到这个组
    unsafe fn push(&self, subdevice: SubDevice) -> Result<(), Error>;

    /// Get a reference to the group with const generic params erased.
    fn as_ref(&self) -> SubDeviceGroupRef<'_>;
}

#[sealed::sealed] // 保证 SubDeviceGroupHandle trait 只能由特定的类型实现，也就是 SubDeviceGroup 类型，从而限制了该 trait 的实现范围
impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, S> SubDeviceGroupHandle
    for SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, S>
where
    S: Sync,
{
    // 获取组ID
    fn id(&self) -> GroupId {
        self.id // 不是原子变量，可以线程安全吗
    }

    // 添加一个从站到这个组
    unsafe fn push(&self, subdevice: SubDevice) -> Result<(), Error> {
        unsafe { (*self.inner.get()).subdevices.push(subdevice) }
            .map_err(|_| Error::Capacity(crate::error::Item::SubDevice))
    }

    fn as_ref(&self) -> SubDeviceGroupRef<'_> {
        SubDeviceGroupRef {
            max_pdi_len: MAX_PDI,
            inner: {
                let inner = unsafe { fmt::unwrap_opt!(self.inner.get().as_mut()) };

                GroupInnerRef {
                    subdevices: &mut inner.subdevices,
                    pdi_start: &mut inner.pdi_start,
                }
            },
        }
    }
}

#[derive(Debug)]
struct GroupInnerRef<'a> {
    subdevices: &'a mut [SubDevice],
    pdi_start: &'a mut PdiOffset,
}

/// A reference to a [`SubDeviceGroup`](crate::SubDeviceGroup) returned by the closure passed to
/// [`MainDevice::init`](crate::MainDevice::init).
#[doc(alias = "SlaveGroupRef")]
pub struct SubDeviceGroupRef<'a> {
    /// Maximum PDI length in bytes.
    max_pdi_len: usize,
    inner: GroupInnerRef<'a>,
}

impl SubDeviceGroupRef<'_> {
    /// Initialise all SubDevices in the group and place them in PRE-OP.
    // Clippy: shush
    #[allow(clippy::wrong_self_convention)]
    pub(crate) async fn into_pre_op<'sto>(
        &mut self,
        pdi_position: PdiOffset,
        maindevice: &'sto MainDevice<'sto>,
    ) -> Result<PdiOffset, Error> {
        let inner = &mut self.inner;

        // Set the starting position in the PDI for this group's segment
        *inner.pdi_start = pdi_position;

        fmt::debug!(
            "Going to configure group with {} SubDevice(s), starting PDI offset {:#08x}",
            inner.subdevices.len(),
            inner.pdi_start.start_address
        );

        // Configure master read PDI mappings in the first section of the PDI
        for subdevice in inner.subdevices.iter_mut() {
            let mut subdevice_config =
                SubDeviceRef::new(maindevice, subdevice.configured_address(), subdevice);

            // TODO: Move PRE-OP transition out of this so we can do it for the group just once
            subdevice_config.configure_mailboxes().await?;
        }

        Ok(pdi_position.increment(self.max_pdi_len as u16))
    }
}
