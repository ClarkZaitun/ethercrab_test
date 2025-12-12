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
fn main() -> Result<(), ethercrab::error::Error> {
    use env_logger::{Env, TimestampPrecision};
    use ethercrab::{MainDevice, MainDeviceConfig, PduStorage, Timeouts, std::ethercat_now};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio::time::MissedTickBehavior;

    // Set process to real-time scheduling with FIFO policy and priority 49
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 49;

        let result = libc::sched_setscheduler(
            0, // 0 means current process
            libc::SCHED_FIFO,
            &param as *const libc::sched_param,
        );

        if result != 0 {
            eprintln!(
                "Failed to set real-time scheduling policy: {}",
                std::io::Error::last_os_error()
            );
            eprintln!("You may need to adjust limits in /etc/security/limits.conf");
            eprintln!("Add lines: <user> hard rtprio 99 and <user> soft rtprio 99");
        } else {
            println!("Successfully set process to FIFO scheduling with priority 49");
        }
    }

    // Create a multi-threaded tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        /// Maximum number of SubDevices that can be stored. This must be a power of 2 greater than 1.
        const MAX_SUBDEVICES: usize = 16;
        /// Maximum PDU data payload size - set this to the max PDI size or higher.
        const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
        /// Maximum number of EtherCAT frames that can be in flight at any one time.
        const MAX_FRAMES: usize = 16;
        /// Maximum total PDI length.
        const PDI_LEN: usize = 64;
        /// Interval in microseconds.
        const INTERVAL: u64 = 1000;

        static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

        env_logger::Builder::from_env(Env::default().default_filter_or("info"))
            .format_timestamp(Some(TimestampPrecision::Nanos))
            .init();

        let interface = std::env::args()
            .nth(1)
            .expect("Provide network interface as first argument.");

        log::info!("Starting single group demo with tokio (process-level FIFO scheduling, priority 49)...");
        log::info!(
            "Ensure an EK1100 or EK1501 is the first SubDevice, with any number of modules connected after"
        );
        log::info!("Run with RUST_LOG=ethercrab=debug or =trace for debug information");

        let (tx, rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

        // Spawn the TX/RX task
        tokio::spawn(async move {
            if let Ok(task) = ethercrab::std::tx_rx_task(&interface, tx, rx) {
                futures_lite::future::block_on(task).expect("TX/RX task");
            } else {
                eprintln!("Failed to create TX/RX task");
                panic!("Failed to create TX/RX task");
            }
        });

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
    });

    Ok(())
}

// [2025-12-12T03:46:07.334213124Z INFO  tokio_single_group] Actual cycle time: 1.164632ms (expected: 1ms)
// [2025-12-12T03:46:07.335371253Z INFO  tokio_single_group] Actual cycle time: 1.158312ms (expected: 1ms)
// [2025-12-12T03:46:07.336531098Z INFO  tokio_single_group] Actual cycle time: 1.159843ms (expected: 1ms)
// [2025-12-12T03:46:07.336772023Z INFO  tokio_single_group] Actual cycle time: 240.922µs (expected: 1ms)
// [2025-12-12T03:46:07.337890108Z INFO  tokio_single_group] Actual cycle time: 1.118053ms (expected: 1ms)
// [2025-12-12T03:46:07.339020360Z INFO  tokio_single_group] Actual cycle time: 1.130389ms (expected: 1ms)
// [2025-12-12T03:46:07.340190981Z INFO  tokio_single_group] Actual cycle time: 1.170209ms (expected: 1ms)
// [2025-12-12T03:46:07.341353247Z INFO  tokio_single_group] Actual cycle time: 1.162504ms (expected: 1ms)
// [2025-12-12T03:46:07.342511314Z INFO  tokio_single_group] Actual cycle time: 1.157942ms (expected: 1ms)
// [2025-12-12T03:46:07.342752982Z INFO  tokio_single_group] Actual cycle time: 242.242µs (expected: 1ms)
// [2025-12-12T03:46:07.343889024Z INFO  tokio_single_group] Actual cycle time: 1.136043ms (expected: 1ms)
// [2025-12-12T03:46:07.345060753Z INFO  tokio_single_group] Actual cycle time: 1.17055ms (expected: 1ms)
// [2025-12-12T03:46:07.346232608Z INFO  tokio_single_group] Actual cycle time: 1.172211ms (expected: 1ms)
// [2025-12-12T03:46:07.347396569Z INFO  tokio_single_group] Actual cycle time: 1.164661ms (expected: 1ms)
// [2025-12-12T03:46:07.348555457Z INFO  tokio_single_group] Actual cycle time: 1.158967ms (expected: 1ms)
// [2025-12-12T03:46:07.348795892Z INFO  tokio_single_group] Actual cycle time: 240.485µs (expected: 1ms)
// [2025-12-12T03:46:07.349952031Z INFO  tokio_single_group] Actual cycle time: 1.156077ms (expected: 1ms)
