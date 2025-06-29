//! Configuration passed to [`MainDevice`](crate::MainDevice).

/// Configuration passed to [`MainDevice`](crate::MainDevice).
// 主站的配置结构体。这里只保存DC相关配置。可以用于保存ENI信息
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MainDeviceConfig {
    /// The number of `FRMW` packets to send during the static phase of Distributed Clocks (DC)
    /// synchronisation.
    ///
    /// Defaults to 10000.
    ///
    /// If this is set to zero, no static sync will be performed.
    // 时钟漂移补偿默认做1000次。规范建议做15000次，越多启动时间就会被拖慢
    pub dc_static_sync_iterations: u32,

    /// EtherCAT packet (PDU) network retry behaviour.
    //默认值是RetryBehaviour::None
    pub retry_behaviour: RetryBehaviour,
}
// Default 是标准库 std::default 模块定义的一个 trait，其作用是为类型提供默认值。若某个类型实现了 Default trait，就能够借助 Default::default() 方法或者 T::default() 语法获取该类型的默认实例
impl Default for MainDeviceConfig {
    fn default() -> Self {
        Self {
            dc_static_sync_iterations: 10_000,
            retry_behaviour: RetryBehaviour::default(), //默认值是RetryBehaviour::None
        }
    }
}

//这是个好设计
/// Network communication retry policy.
///
/// Retries will be performed at the rate defined by [`Timeouts::pdu`](crate::Timeouts::pdu).
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub enum RetryBehaviour {
    /// Do not attempt to retry timed out packet sends (default).
    ///
    /// If this option is chosen, any timeouts will raise an
    /// [`Error::Timeout`](crate::error::Error::Timeout).
    #[default]
    None,

    /// Attempt to resend a PDU up to `N` times, then raise an
    /// [`Error::Timeout`](crate::error::Error::Timeout).
    Count(usize),

    /// Attempt to resend the PDU forever(*).
    ///
    /// Note that this can soft-lock a program if for example the EtherCAT network cable is removed
    /// as EtherCrab will attempt to resend the packet forever. It may be preferable to use
    /// [`RetryBehaviour::Count`] to set an upper bound on retries.
    ///
    /// (*) Forever in this case means a retry count of `usize::MAX`.
    Forever,
}

impl RetryBehaviour {
    pub(crate) const fn retry_count(&self) -> usize {
        match self {
            // Try at least once when used in a range like `for _ in 0..<counts>`.
            RetryBehaviour::None => 0,
            RetryBehaviour::Count(n) => *n,
            RetryBehaviour::Forever => usize::MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_count_sanity_check() {
        assert_eq!(RetryBehaviour::None.retry_count(), 0);
        assert_eq!(RetryBehaviour::Count(10).retry_count(), 10);
        assert_eq!(RetryBehaviour::Forever.retry_count(), usize::MAX);
    }
}
