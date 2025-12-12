//! More precise test to measure tokio::spawn overhead using high-resolution timing

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Measuring tokio::spawn overhead with high-resolution timing...\n");

    // Warm up the runtime
    for _ in 0..1000 {
        let h = tokio::spawn(async {});
        h.await.unwrap();
    }

    // Test 1: Measure spawn overhead with minimal work
    println!("Test 1: Spawn overhead with minimal work");
    let iterations = 100000u32;
    let start = Instant::now();

    for i in 0..iterations {
        let h = tokio::spawn(async move { i });
        let result = h.await.unwrap();
        assert_eq!(result, i);
    }

    let duration = start.elapsed();
    let avg_duration = duration / iterations;

    println!("  Total time for {} spawns: {:?}", iterations, duration);
    println!("  Average time per spawn: {:?}", avg_duration);
    println!(
        "  Spawns per second: {:.0}\n",
        iterations as f64 / duration.as_secs_f64()
    );

    // Test 2: Measure spawn overhead with counter increment
    println!("Test 2: Spawn overhead with atomic counter increment");
    let iterations = 100000u32;
    let start = Instant::now();

    for _ in 0..iterations {
        let h = tokio::spawn(async { COUNTER.fetch_add(1, Ordering::Relaxed) });
        h.await.unwrap();
    }

    let duration = start.elapsed();
    let avg_duration = duration / iterations;

    println!("  Total time for {} spawns: {:?}", iterations, duration);
    println!("  Average time per spawn: {:?}", avg_duration);
    println!(
        "  Spawns per second: {:.0}\n",
        iterations as f64 / duration.as_secs_f64()
    );

    // Test 3: Compare with direct function call
    println!("Test 3: Direct function call (baseline)");
    let iterations = 100000u32;
    let start = Instant::now();

    for i in 0..iterations {
        let result = direct_call(i);
        assert_eq!(result, i);
    }

    let duration = start.elapsed();
    let avg_duration = duration / iterations;

    println!(
        "  Total time for {} direct calls: {:?}",
        iterations, duration
    );
    println!("  Average time per call: {:?}", avg_duration);
    println!(
        "  Calls per second: {:.0}\n",
        iterations as f64 / duration.as_secs_f64()
    );

    // Test 4: Measure spawn overhead with small sleep
    println!("Test 4: Spawn overhead with small sleep (1μs)");
    let iterations = 10000u32;
    let start = Instant::now();

    for i in 0..iterations {
        let h = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_micros(1)).await;
            i as u32
        });
        let result = h.await.unwrap();
        assert_eq!(result, i);
    }

    let duration = start.elapsed();
    let avg_duration = duration / iterations;

    println!(
        "  Total time for {} spawns with sleep: {:?}",
        iterations, duration
    );
    println!("  Average time per spawn: {:?}", avg_duration);
    println!(
        "  Spawns per second: {:.0}\n",
        iterations as f64 / duration.as_secs_f64()
    );

    Ok(())
}

fn direct_call(i: u32) -> u32 {
    i
}
