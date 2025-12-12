//! Test to measure smol::spawn overhead

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Smol spawn overhead test...\n");

    // Warm up the runtime
    for _ in 0..100 {
        let h = smol::spawn(async {});
        let _ = smol::block_on(h);
    }

    // Test spawn overhead with minimal work
    println!("Testing smol::spawn overhead...");
    let iterations = 10000;
    let start = Instant::now();

    for i in 0..iterations {
        let h = smol::spawn(async move { i });
        let result = smol::block_on(h);
        assert_eq!(result, i);
    }

    let duration = start.elapsed();
    let avg_duration = duration / iterations;

    println!("Total time for {} spawns: {:?}", iterations, duration);
    println!("Average time per spawn: {:?}", avg_duration);
    println!(
        "Spawns per second: {:.0}",
        iterations as f64 / duration.as_secs_f64()
    );

    // Compare with direct function call
    println!("\nTesting direct function call (baseline)...");
    let start = Instant::now();

    for i in 0..iterations {
        let result = i;
        assert_eq!(result, i);
    }

    let duration = start.elapsed();
    let avg_duration = duration / iterations;

    println!("Total time for {} direct calls: {:?}", iterations, duration);
    println!("Average time per call: {:?}", avg_duration);
    println!(
        "Calls per second: {:.0}",
        iterations as f64 / duration.as_secs_f64()
    );

    Ok(())
}
