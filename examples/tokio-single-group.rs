//! Use blocking socket-based TX/RX loop with async tasks executed with `tokio` and a single group.
//!
//! This example pins the TX/RX loop to core 0, starts tasks on the main thread.
//!
//! You may need to increase `INTERVAL` as 100us can be challenging for some PCs. That said, a
//! Raspberry Pi 4 with a realtime kernel and some tweaking can run 2x 100us tasks _ok_.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This example is only supported on Linux systems");
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), ethercrab::error::Error> {
    use env_logger::{Env, TimestampPrecision};
    use ethercrab::{MainDevice, MainDeviceConfig, PduStorage, Timeouts, std::ethercat_now};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use thread_priority::{
        RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
    };
    use tokio::time::MissedTickBehavior;

    /// Maximum number of SubDevices that can be stored. This must be a power of 2 greater than 1.
    const MAX_SUBDEVICES: usize = 16;
    /// Maximum PDU data payload size - set this to the max PDI size or higher.
    const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
    /// Maximum number of EtherCAT frames that can be in flight at any one time.
    const MAX_FRAMES: usize = 16;
    /// Maximum total PDI length.
    const PDI_LEN: usize = 64;
    /// Interval in microseconds.
    const INTERVAL: u64 = 100;

    static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp(Some(TimestampPrecision::Nanos))
        .init();

    let interface = std::env::args()
        .nth(1)
        .expect("Provide network interface as first argument.");

    log::info!("Starting single group demo with tokio (multi-threaded)...");
    log::info!(
        "Ensure an EK1100 or EK1501 is the first SubDevice, with any number of modules connected after"
    );
    log::info!("Run with RUST_LOG=ethercrab=debug or =trace for debug information");

    let (tx, rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

    let core_ids = core_affinity::get_core_ids().expect("Couldn't get core IDs");

    let tx_rx_core = core_ids
        .first()
        .copied()
        .expect("At least one core is required. Are you running on a potato?");

    // Spawn the TX/RX task on a separate thread
    let handle = thread_priority::ThreadBuilder::default()
        .name("tx-rx-thread")
        // Might need to set `<user> hard rtprio 99` and `<user> soft rtprio 99` in `/etc/security/limits.conf`
        // Check limits with `ulimit -Hr` or `ulimit -Sr`
        .priority(ThreadPriority::Crossplatform(
            ThreadPriorityValue::try_from(49u8).unwrap(),
        ))
        // NOTE: Requires a realtime kernel
        .policy(ThreadSchedulePolicy::Realtime(
            RealtimeThreadSchedulePolicy::Fifo,
        ))
        .spawn(move |_| {
            core_affinity::set_for_current(tx_rx_core)
                .then_some(())
                .expect("Set TX/RX thread core");

            // libc socket
            match ethercrab::std::tx_rx_task(&interface, tx, rx) {
                Ok(task) => {
                    futures_lite::future::block_on(task).expect("TX/RX task");
                }
                Err(e) => {
                    eprintln!(
                        "Failed to create TX/RX task for interface '{}': {}",
                        interface, e
                    );
                    eprintln!("Possible causes:");
                    eprintln!(
                        "1. Interface '{}' does not exist or is not available",
                        interface
                    );
                    eprintln!(
                        "2. Insufficient permissions (try running with sudo or setting cap_net_raw)"
                    );
                    eprintln!("3. Interface is not an Ethernet interface");
                    panic!("Failed to create TX/RX task");
                }
            }
        })
        .unwrap();

    let maindevice = MainDevice::new(
        pdu_loop,
        Timeouts {
            // Enormous timeout so we can still keep going even with very high system load
            // preventing processing from happening.
            pdu: Duration::from_millis(1000),
            ..Timeouts::default()
        },
        MainDeviceConfig::default(),
    );

    let maindevice = Arc::new(maindevice);

    // Read configurations from SubDevice EEPROMs and configure devices.
    let group = maindevice
        .init_single_group::<MAX_SUBDEVICES, PDI_LEN>(ethercat_now)
        .await
        .expect("Init");

    log::info!("Discovered {} SubDevices", group.len());

    let group = group.into_op(&maindevice).await.expect("PRE-OP -> OP");

    for subdevice in group.iter(&maindevice) {
        let io = subdevice.io_raw();

        log::info!(
            "-> SubDevice {:#06x} {} inputs: {} bytes, outputs: {} bytes",
            subdevice.configured_address(),
            subdevice.name(),
            io.inputs().len(),
            io.outputs().len()
        );
    }

    let maindevice_clone = maindevice.clone();

    // Create interval timer for cyclic task
    let mut interval = tokio::time::interval(Duration::from_micros(INTERVAL));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // 添加时间测量
    let mut last_time = Instant::now();

    loop {
        let Ok(_) = group.tx_rx(&maindevice_clone).await else {
            break;
        };

        // 测量实际周期时间
        let current_time = Instant::now();
        let elapsed = current_time.duration_since(last_time);
        last_time = current_time;

        // [2025-12-12T02:41:07.465343268Z INFO  smol_io_uring_single_group] Actual cycle time: 246.557µs (expected: 100µs)
        // 抓包和打印都证明没有达到100µs
        log::info!(
            "Actual cycle time: {:?} (expected: {:?})",
            elapsed,
            Duration::from_micros(INTERVAL)
        );

        // Increment every output byte for every SubDevice by one
        for subdevice in group.iter(&maindevice_clone) {
            let mut o = subdevice.outputs_raw_mut();

            for byte in o.iter_mut() {
                *byte = byte.wrapping_add(1);
            }
        }

        interval.tick().await;
    }

    // Wait for the TX/RX thread to complete (though it runs indefinitely)
    handle.join().unwrap();

    Ok(())
}
