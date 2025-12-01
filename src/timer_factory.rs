use crate::error::{Error, TimeoutError};
use core::{future::Future, pin::Pin, task::Poll, time::Duration};

#[cfg(not(feature = "std"))]
pub(crate) type Timer = embassy_time::Timer;
#[cfg(all(not(miri), feature = "std"))]
pub(crate) type Timer = async_io::Timer;
#[cfg(miri)]
// #[cfg(miri)]是Rust的一个内置条件编译属性，它不需要在Cargo.toml中显式配置。Miri是Rust的官方解释器和运行时验证工具，用于测试和验证代码的安全性和正确性。
// 没有配置 miri时，IDE会错误地显示 miri
//在 Miri 环境下，把 Timer 类型别名为一个永远不会完成的 Future，以此模拟定时器操作，确保代码在 Miri 内存安全检查时能正常运行。
pub(crate) type Timer = core::future::Pending<()>;

// 创建一个定时器
#[cfg(not(feature = "std"))]
pub(crate) fn timer(timeout: LabeledTimeout) -> Timer {
    embassy_time::Timer::after(embassy_time::Duration::from_micros(
        timeout.duration.as_micros() as u64,
    ))
}
// 创建一个定时器
#[cfg(all(not(miri), feature = "std"))]
pub(crate) fn timer(timeout: LabeledTimeout) -> Timer {
    // 创建了一个异步定时器 Future，它会在指定的 duration 时间后完成
    // 这是 Rust 异步编程中实现非阻塞延迟的标准方式。
    // 非阻塞行为：不会阻塞调用线程，而是允许异步运行时调度其他任务
    // 协作式调度：当 .await 这个定时器时，执行会让出控制权，直到指定时间过去
    // 资源高效：相比传统的线程睡眠，这种方式可以在单线程上高效处理大量并发定时操作
    async_io::Timer::after(timeout.duration)
}

// 创建一个定时器
#[cfg(miri)]
pub(crate) fn timer(_timeout: LabeledTimeout) -> Timer {
    core::future::pending()
}

pub(crate) trait IntoTimeout<O> {
    fn timeout(
        self,
        timeout: LabeledTimeout,
    ) -> TimeoutFuture<impl Future<Output = Result<O, Error>>>;
}

impl<T, O> IntoTimeout<O> for T
where
    T: Future<Output = Result<O, Error>>,
{
    // 将原始 Future (self) 包装成一个带超时的 TimeoutFuture
    fn timeout(
        self,
        timeout: LabeledTimeout,
    ) -> TimeoutFuture<impl Future<Output = Result<O, Error>>> {
        TimeoutFuture {
            f: self,
            timeout: timer(timeout),
            duration: timeout,
        }
    }
}

pub(crate) struct TimeoutFuture<F> {
    f: F,
    timeout: Timer,
    duration: LabeledTimeout,
}

impl<F, O> Future for TimeoutFuture<F>
where
    F: Future<Output = Result<O, Error>>,
{
    type Output = Result<O, Error>;

    // 这是一个异步超时包装器的轮询方法，用于为任何返回 Result<O, Error> 的 Future 添加超时功能。
    // 当原始 Future 在指定时间内未完成时，它会提前返回超时错误。
    // 超时优先：首先检查超时定时器是否就绪，如果已超时立即返回 Err(Error::Timeout)
    // 完成检查：其次检查原始 Future 是否完成，若完成则直接返回其结果
    // 挂起等待：如果两者都未完成，则返回 Poll::Pending，等待下次唤醒

    // poll 方法会在以下情况被调用：
    // 异步运行时调度：当 tokio、futures_lite 等异步运行时调度该 future 执行时
    // 唤醒事件触发：当关联的 waker 被唤醒时（如定时器过期、底层 future 就绪）
    // 手动轮询：在测试代码中通过 poll_fn 或类似机制手动轮询时
    fn poll(self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> Poll<Self::Output> {
        // 1. 获取可变引用并创建固定指针
        let this = unsafe { self.get_unchecked_mut() };
        let timeout = unsafe { Pin::new_unchecked(&mut this.timeout) };
        let f = unsafe { Pin::new_unchecked(&mut this.f) };

        // 2. Miri 特定处理：零超时情况
        #[cfg(miri)]
        if this.duration.duration == Duration::ZERO {
            return Poll::Ready(Err(Error::Timeout(TimeoutError::from_timeout_kind(
                this.duration.kind,
            ))));
        }

        // 3. 检查是否超时
        if timeout.poll(cx).is_ready() {
            return Poll::Ready(Err(Error::Timeout(TimeoutError::from_timeout_kind(
                this.duration.kind,
            ))));
        }

        // 4. 检查 TimeoutFuture 内部的原始 Future 是否完成
        if let Poll::Ready(x) = f.poll(cx) {
            return Poll::Ready(x);
        }

        Poll::Pending
    }
}

/// Timeout configuration for the EtherCrab master.
//用于配置不同操作的超时时间。这个部分应该改为保留默认值，可由ENI提供，或由用户自行配置。
#[derive(Copy, Clone, Debug)]
pub struct Timeouts {
    // 不同状态的切换超时时间不同，所以这里设计不完美。只能取最大值
    /// How long to wait for a SubDevice state change, e.g. SAFE-OP to OP.
    ///
    /// This timeout is global for all state transitions.
    pub state_transition: Duration,

    /// How long to wait for a PDU response.
    pub pdu: Duration,

    /// How long to wait for a single EEPROM operation.
    pub eeprom: Duration,

    /// Polling duration of wait loops.
    ///
    /// Some operations require repeatedly reading something from a SubDevice until a value changes.
    /// This duration specifies the wait time between polls.
    ///
    /// This defaults to a timeout of 0 to keep latency to a minimum.
    // 某些操作需要反复从子设备读取数据，直到值发生变化。
    // 这个值指定轮询之间的等待时间。
    pub wait_loop_delay: Duration,

    /// How long to wait for a SubDevice mailbox to become ready.
    //等待从站邮箱准备好超时时间
    pub mailbox_echo: Duration,

    // 等待邮箱响应超时时间
    /// How long to wait for a response to be read from the SubDevice's response mailbox.
    pub mailbox_response: Duration,
}

/// The kinds of timeouts that can be awaited for an EtherCAT bus.
///
/// See [`Timeouts`] for what each timeout is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum TimeoutKind {
    StateTransition,
    Pdu,
    Eeprom,
    WaitLoopDelay,
    MailboxEcho,
    MailboxResponse,
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct LabeledTimeout {
    pub duration: Duration,
    pub kind: TimeoutKind,
}

impl Timeouts {
    // 轮询间隔控制函数，用于在需要重复轮询设备状态或等待条件满足的场景中，提供可配置的时间间隔。它确保在轮询操作之间有适当的延迟，以平衡系统响应性和资源使用。
    pub(crate) async fn loop_tick(&self) {
        #[cfg(not(miri))]
        timer(self.wait_loop_delay()).await; // 调用 timer 函数创建一个定时器，异步等待指定时间(timer(self.wait_loop_delay).await) // 异步等待指定时间(timer(self.wait_loop_delay).await)
        #[cfg(miri)]
        std::thread::yield_now();
    }

    /// Get the timeout for a state transition.
    pub(crate) fn state_transition(self) -> LabeledTimeout {
        LabeledTimeout {
            duration: (self.state_transition),
            kind: TimeoutKind::StateTransition,
        }
    }
    /// Get the timeout for a PDU.
    pub(crate) fn pdu(self) -> LabeledTimeout {
        LabeledTimeout {
            duration: (self.pdu),
            kind: TimeoutKind::Pdu,
        }
    }
    /// Get the timeout for the EEPROM.
    pub(crate) fn eeprom(self) -> LabeledTimeout {
        LabeledTimeout {
            duration: (self.eeprom),
            kind: TimeoutKind::Eeprom,
        }
    }
    /// Get the timeout for a wait loop delay.
    pub(crate) fn wait_loop_delay(self) -> LabeledTimeout {
        LabeledTimeout {
            duration: (self.wait_loop_delay),
            kind: TimeoutKind::WaitLoopDelay,
        }
    }
    /// Get the timeout for a mailbox echo.
    pub(crate) fn mailbox_echo(self) -> LabeledTimeout {
        LabeledTimeout {
            duration: (self.mailbox_echo),
            kind: TimeoutKind::MailboxEcho,
        }
    }
    /// Get the timeout for a mailbox response.
    pub(crate) fn mailbox_response(self) -> LabeledTimeout {
        LabeledTimeout {
            duration: (self.mailbox_response),
            kind: TimeoutKind::MailboxResponse,
        }
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            state_transition: Duration::from_millis(5000),
            pdu: Duration::from_micros(30_000),
            eeprom: Duration::from_millis(10),
            wait_loop_delay: Duration::from_millis(0),
            mailbox_echo: Duration::from_millis(100),
            mailbox_response: Duration::from_millis(1000),
        }
    }
}

// Timeouts used for testing
#[cfg(test)]
pub(crate) const MAX_TIMEOUT: crate::timer_factory::LabeledTimeout =
    crate::timer_factory::LabeledTimeout {
        duration: Duration::MAX,
        kind: crate::timer_factory::TimeoutKind::Pdu,
    };
#[cfg(test)]
pub(crate) const MIN_TIMEOUT: crate::timer_factory::LabeledTimeout =
    crate::timer_factory::LabeledTimeout {
        duration: Duration::ZERO,
        kind: crate::timer_factory::TimeoutKind::Pdu,
    };
