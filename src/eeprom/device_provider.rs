use crate::{
    Command, MainDevice,
    eeprom::{
        EepromDataProvider,
        types::{SiiControl, SiiRequest},
    },
    error::{EepromError, Error},
    fmt,
    register::RegisterAddress,
    timer_factory::IntoTimeout,
};

// 第一个category的字地址
/// The address of the first proper category, positioned after the fixed fields defined in ETG2010
/// Table 2.
///
/// SII EEPROM is WORD-addressed.
pub(crate) const SII_FIRST_CATEGORY_START: u16 = 0x0040u16;

// DeviceEeprom类型实现了EepromDataProvider trait
/// EEPROM data provider that communicates with a physical sub device.
#[derive(Clone)]
pub struct DeviceEeprom<'subdevice> {
    maindevice: &'subdevice MainDevice<'subdevice>,
    configured_address: u16,
}

impl<'subdevice> DeviceEeprom<'subdevice> {
    // DeviceEeprom类型实现了EepromDataProvider trait
    /// Create a new EEPROM reader instance.
    pub fn new(maindevice: &'subdevice MainDevice<'subdevice>, configured_address: u16) -> Self {
        Self {
            maindevice,
            configured_address,
        }
    }

    // 等待 EEPROM 空闲，或者超时
    async fn wait_while_busy(&self) -> Result<SiiControl, Error> {
        let res = async {
            loop {
                let control: SiiControl =
                    Command::fprd(self.configured_address, RegisterAddress::SiiControl.into())
                        .receive::<SiiControl>(self.maindevice)
                        .await?;

                // 检查SII是否忙，不忙跳出循环
                if !control.busy {
                    break Ok(control);
                }

                self.maindevice.timeouts.loop_tick().await;
            }
        }
        .timeout(self.maindevice.timeouts.eeprom)
        .await?;

        Ok(res)
    }
}

impl EepromDataProvider for DeviceEeprom<'_> {
    // 从EEPROM中读取4或8字节数据
    async fn read_chunk(
        &mut self,
        start_word: u16,
    ) -> Result<impl core::ops::Deref<Target = [u8]>, Error> {
        // FPWR 0x0502 读取位置1：发送读取请求
        Command::fpwr(self.configured_address, RegisterAddress::SiiControl.into())
            .send(self.maindevice, SiiRequest::read(start_word))
            .await?;

        // 等待 EEPROM 空闲，或者超时
        let status = self.wait_while_busy().await?;

        // FPRD 0x0508 获取EEPROM数据
        Command::fprd(self.configured_address, RegisterAddress::SiiData.into())
            .receive_slice(self.maindevice, status.read_size.chunk_len()) // 根据EEPROM允许一次读取4或8字节，决定读取数据的长度
            .await
            .inspect(|data| {
                #[cfg(not(feature = "defmt"))]
                fmt::trace!("Read addr {:#06x}: {:02x?}", start_word, &data[..]);
                #[cfg(feature = "defmt")]
                fmt::trace!("Read addr {:#06x}: {=[u8]}", start_word, &data[..]);
            })
    }

    async fn write_word(&mut self, start_word: u16, data: [u8; 2]) -> Result<(), Error> {
        // Check if the EEPROM is busy
        self.wait_while_busy().await?;

        let mut retry_count = 0;

        loop {
            // Set data to write
            Command::fpwr(self.configured_address, RegisterAddress::SiiData.into())
                .send(self.maindevice, data)
                .await?;

            // Send control and address registers. A rising edge on the write flag will store whatever
            // is in `SiiAddress` into the EEPROM at the given address.
            Command::fpwr(self.configured_address, RegisterAddress::SiiControl.into())
                .send(self.maindevice, SiiRequest::write(start_word))
                .await?;

            // Wait for error or not busy
            let status = self.wait_while_busy().await?;

            if status.command_error && retry_count < 20 {
                fmt::debug!("Retrying EEPROM write");

                retry_count += 1;
            } else {
                break;
            }
        }

        Ok(())
    }

    // 尝试清除 EEPROM 数据源中的错误
    async fn clear_errors(&self) -> Result<(), Error> {
        // FPRD 0x0502
        let status = Command::fprd(self.configured_address, RegisterAddress::SiiControl.into())
            .receive::<SiiControl>(self.maindevice)
            .await?;

        // Clear errors
        let status = if status.has_error() {
            fmt::trace!("Resetting EEPROM error flags");

            // FPWR 0x0502 清除故障。会检查WKC
            // 访问单个从站时，默认给预期WKC设置为1
            Command::fpwr(self.configured_address, RegisterAddress::SiiControl.into())
                .send_receive(self.maindevice, status.error_reset())
                .await?
        } else {
            status
        };

        // 再次检查新的状态是否包含错误标志
        if status.has_error() {
            Err(Error::Eeprom(EepromError::ClearErrors))
        } else {
            Ok(())
        }
    }
}
