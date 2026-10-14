//! Runs a PTP clock and prints its status once a second.
//!
//! ```text
//! ptp_follow <interface> [skew_ppm] [offset_ms] [seconds] [priority1]
//! ```
//!
//! `skew_ppm` and `offset_ms` make this node's system clock run fast and
//! off, to watch the servo correct it. Needs CAP_NET_BIND_SERVICE, e.g. in
//! `unshare -Urn`.

use std::time::Duration;

use orpheus_clock::ptp::{PtpClock, PtpConfig};
use orpheus_clock::{Clock, SystemClock, Timescale};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, default: f64| args.get(i).map_or(default, |s| s.parse().unwrap());
    let interface = args.get(1).cloned();
    let (skew, offset_ms, seconds) = (arg(2, 0.0), arg(3, 0.0), arg(4, 30.0) as u64);
    let cfg = PtpConfig {
        interface,
        priority1: arg(5, 250.0) as u8,
        ..Default::default()
    };
    let timescale = Timescale::tai().skewed(skew, (offset_ms * 1e6) as i64);
    let clock = PtpClock::start(cfg, timescale).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    let system = SystemClock::new();
    for _ in 0..seconds {
        std::thread::sleep(Duration::from_secs(1));
        let s = clock.status();
        let vs_system = (clock.now_ns() as i64 - system.now_ns() as i64) as f64 / 1e6;
        println!(
            "{:?} settled={} offset_ns={:?} delay_ns={:?} freq_ppm={:.2} steps={} gm={} ptp-system={vs_system:.3}ms",
            s.state,
            s.settled,
            s.offset_ns,
            s.mean_delay_ns,
            s.frequency_ppm,
            s.steps,
            s.grandmaster
        );
    }
}
