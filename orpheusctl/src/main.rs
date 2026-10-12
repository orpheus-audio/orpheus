//! `orpheusctl`: command-line client for any Orpheus node.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Base URL of the node to talk to.
    #[arg(long, default_value = "http://localhost:7480", env = "ORPHEUS_NODE")]
    node: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the node's ID, name, version and streams.
    Status,
    /// Send the node's audio to a peer: creates a sink "Orpheus → <name>"
    /// whose audio is streamed to the peer.
    Connect {
        /// Peer: host name or address, optionally with :port (default 5004);
        /// IPv6 with a port as [addr]:port.
        peer: String,
        /// Name shown in the sink; defaults to the peer address.
        #[arg(long)]
        name: Option<String>,
    },
    /// Stop a sender started with `connect`.
    Disconnect {
        /// Sender ID, as shown by `status`.
        id: u32,
    },
}

#[derive(Deserialize)]
struct Status {
    id: String,
    name: String,
    version: String,
    #[serde(default = "yes")]
    audio: bool,
    clock: Option<ClockInfo>,
    loopback: Option<Buffer>,
    receiver: Option<Receiver>,
    #[serde(default)]
    senders: Vec<Sender>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct ClockInfo {
    source: String,
    ptp: Option<Ptp>,
}

fn ipv4() -> String {
    "ipv4".into()
}

#[derive(Deserialize)]
struct Ptp {
    interface: String,
    /// Missing from older nodes, which only spoke IPv4.
    #[serde(default = "ipv4")]
    transport: String,
    identity: String,
    state: String,
    grandmaster: String,
    offset_ns: Option<i64>,
    mean_delay_ns: Option<i64>,
    frequency_ppm: f64,
    steps: u64,
    settled: bool,
}

#[derive(Deserialize)]
struct Buffer {
    underruns: u64,
    overruns: u64,
    resyncs: u64,
    level_frames: u64,
}

#[derive(Deserialize)]
struct Receiver {
    listen: String,
    link_offset_ms: u32,
    ssrc: Option<u32>,
    packets: u64,
    malformed: u64,
    late: u64,
    early: u64,
    lost: u64,
    realigns: u64,
    margin_us: Option<i64>,
    #[serde(default)]
    asrc_ppm: f64,
    buffer: Buffer,
}

#[derive(Deserialize)]
struct Sender {
    id: u32,
    peer: String,
    packets: u64,
    send_errors: u64,
    realigns: u64,
    #[serde(default)]
    asrc_ppm: f64,
    buffer: Buffer,
}

#[derive(Serialize)]
struct NewSender {
    peer: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct Created {
    id: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = cli.node.trim_end_matches('/');
    match cli.command {
        Command::Status => {
            let url = format!("{base}/x-orpheus/v1/status");
            let s: Status = ureq::get(&url)
                .call()
                .and_then(|mut r| r.body_mut().read_json())
                .with_context(|| format!("querying {url}"))?;
            println!("{}  {}  (orpheusd {})", s.id, s.name, s.version);
            if !s.audio {
                println!("no audio system");
            }
            match s.clock {
                Some(ClockInfo { ptp: Some(p), .. }) => {
                    let us = |ns: Option<i64>| {
                        ns.map_or("-".into(), |n| format!("{:.1} µs", n as f64 / 1000.0))
                    };
                    let role = if p.grandmaster == p.identity {
                        "grandmaster".to_owned()
                    } else {
                        format!("grandmaster {}", p.grandmaster)
                    };
                    println!(
                        "clock: PTP on {}/{} ({}), {}, {}, offset {}, delay {}, {:+.2} ppm, {} steps, {}",
                        p.interface,
                        p.transport,
                        p.identity,
                        p.state,
                        role,
                        us(p.offset_ns),
                        us(p.mean_delay_ns),
                        p.frequency_ppm,
                        p.steps,
                        if p.settled { "settled" } else { "settling" }
                    );
                }
                Some(c) => println!("clock: {}", c.source),
                None => {}
            }
            if let Some(l) = s.loopback {
                println!("loopback: {}", buffer(&l));
            }
            if let Some(r) = s.receiver {
                let stream = r.ssrc.map_or("idle".into(), |s| format!("ssrc {s:08x}"));
                let margin = r
                    .margin_us
                    .map_or("-".into(), |m| format!("{:.2} ms", m as f64 / 1000.0));
                println!(
                    "receiver {} ({}, link offset {} ms): {} packets, {} lost, {} late, {} early, {} malformed, {} realigns, margin {}",
                    r.listen,
                    stream,
                    r.link_offset_ms,
                    r.packets,
                    r.lost,
                    r.late,
                    r.early,
                    r.malformed,
                    r.realigns,
                    margin
                );
                println!(
                    "  playout: {}, ASRC {:+.1} ppm",
                    buffer(&r.buffer),
                    r.asrc_ppm
                );
            }
            for t in s.senders {
                println!(
                    "sender {} → {}: {} packets, {} send errors, {} realigns",
                    t.id, t.peer, t.packets, t.send_errors, t.realigns
                );
                println!(
                    "  input: {}, ASRC {:+.1} ppm",
                    buffer(&t.buffer),
                    t.asrc_ppm
                );
            }
        }
        Command::Connect { peer, name } => {
            let url = format!("{base}/x-orpheus/v1/senders");
            let created: Created = ureq::post(&url)
                .send_json(NewSender {
                    peer: peer.clone(),
                    name,
                })
                .and_then(|mut r| r.body_mut().read_json())
                .with_context(|| format!("connecting to {peer} via {url}"))?;
            println!("sender {} → {peer}", created.id);
        }
        Command::Disconnect { id } => {
            let url = format!("{base}/x-orpheus/v1/senders/{id}");
            ureq::delete(&url)
                .call()
                .with_context(|| format!("stopping sender {id} via {url}"))?;
        }
    }
    Ok(())
}

fn buffer(b: &Buffer) -> String {
    format!(
        "{} frames buffered, {} underruns, {} overruns, {} resyncs",
        b.level_frames, b.underruns, b.overruns, b.resyncs
    )
}
