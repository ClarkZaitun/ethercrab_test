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
    // 将 SubDeviceGroup 转换为 SubDeviceGroupRef 类型，同时擦除其常量泛型参数
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

    // 将 SubDeviceGroup 转换为 SubDeviceGroupRef 类型，同时擦除其常量泛型参数
    fn as_ref(&self) -> SubDeviceGroupRef<'_> {
        // 创建并返回一个 SubDeviceGroupRef 实例
        SubDeviceGroupRef {
            // 将当前组的最大 PDI 长度赋值给 max_pdi_len 字段
            max_pdi_len: MAX_PDI,
            inner: {
                // 通过 unsafe 块获取内部数据GroupInner的可变引用
                // fmt::unwrap_opt! 宏用于解包 Option 类型的值，如果值为 None 则触发 panic
                let inner = unsafe { fmt::unwrap_opt!(self.inner.get().as_mut()) };

                // 创建 GroupInnerRef 实例。GroupInner转换为GroupInnerRef
                GroupInnerRef {
                    // 获取内部从设备切片的可变引用
                    subdevices: &mut inner.subdevices,
                    // 获取内部 PDI 起始偏移量的可变引用
                    pdi_start: &mut inner.pdi_start,
                }
            },
        }
    }
}

#[derive(Debug)]
struct GroupInnerRef<'a> {
    // 指向 SubDevice 切片的可变引用，生命周期为 'a。
    // 这意味着在 'a 的生命周期内，可以对这个切片中的 SubDevice 元素进行修改操作。
    subdevices: &'a mut [SubDevice],
    // 指向 PdiOffset 实例的可变引用，生命周期为 'a。
    // 这允许在 'a 的生命周期内修改 PdiOffset 实例的内容。
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
    #[allow(clippy::wrong_self_convention)] // 禁用 Clippy 的 wrong_self_convention 警告，可能是因为方法命名不符合 self 参数的常规使用习惯
    // 初始化组内的所有从设备（SubDevice），并将它们置于 PRE-OP状态，同时配置组内从设备在过程数据映像（PDI）中的映射。这里会修改pdi_position的start_address
    // 通过EEOROM数据配置邮箱，切换到PreOp，读取对象字典中的0x1c00同步管理器类型，保存邮箱配置
    pub(crate) async fn into_pre_op<'sto>(
        &mut self,
        pdi_position: PdiOffset,
        maindevice: &'sto MainDevice<'sto>,
    ) -> Result<PdiOffset, Error> {
        let inner = &mut self.inner;

        // Set the starting position in the PDI for this group's segment
        // 设置当前组在 PDI 中的起始偏移量
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
            // 通过EEOROM数据配置邮箱，切换到PreOp，读取对象字典中的0x1c00同步管理器类型，保存邮箱配置
            subdevice_config.configure_mailboxes().await?;
        }

        // start_address 增加 max_pdi_len 字节
        // TODO：给邮箱用还是FMMU用？
        Ok(pdi_position.increment(self.max_pdi_len as u16))
    }
}
