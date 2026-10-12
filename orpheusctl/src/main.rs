//! `orpheusctl`: command-line client for any Orpheus node.

use std::fmt::Write as _;
use std::io::IsTerminal;

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
    /// Send the node's audio to a peer: creates a sink "<name>"
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
            print!("{}", render(&s, Paint::detect()));
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

/// Width of the key column, so that values line up under section headlines.
const KEY: usize = 11;

/// ANSI styling, on only when stdout is a terminal and `NO_COLOR` is unset.
#[derive(Clone, Copy)]
struct Paint(bool);

#[derive(Clone, Copy)]
enum Level {
    Good,
    Warn,
    Bad,
    Off,
}

impl Paint {
    fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self(std::io::stdout().is_terminal() && !no_color)
    }

    fn sgr(self, code: &str, s: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }

    fn bold(self, s: &str) -> String {
        self.sgr("1", s)
    }

    fn dim(self, s: &str) -> String {
        self.sgr("2", s)
    }

    fn level(self, level: Level, s: &str) -> String {
        let code = match level {
            Level::Good => "32",
            Level::Warn => "33",
            Level::Bad => "31",
            Level::Off => "2",
        };
        self.sgr(code, s)
    }

    /// A status dot followed by its meaning, so it reads without colour too.
    fn mark(self, level: Level, s: &str) -> String {
        let dot = if matches!(level, Level::Off) {
            "○"
        } else {
            "●"
        };
        format!("{} {s}", self.level(level, dot))
    }
}

fn heading(o: &mut String, p: Paint, title: &str, mark: Option<String>) {
    let title = format!("{title:<w$}", w = KEY + 4);
    let line = match mark {
        Some(m) => format!("{}{m}", p.bold(&title)),
        None => p.bold(title.trim_end()),
    };
    let _ = writeln!(o, "\n{line}");
}

fn row(o: &mut String, p: Paint, key: &str, value: &str) {
    let _ = writeln!(o, "  {}  {value}", p.dim(&format!("{key:<KEY$}")));
}

fn us(ns: Option<i64>) -> String {
    ns.map_or("-".into(), |n| format!("{:.1} µs", n as f64 / 1000.0))
}

/// The non-zero counters, highlighted, or "none".
fn problems(p: Paint, counts: &[(u64, &str, &str)]) -> String {
    let found: Vec<String> = counts
        .iter()
        .filter(|(n, ..)| *n > 0)
        .map(|&(n, one, many)| format!("{n} {}", if n == 1 { one } else { many }))
        .collect();
    if found.is_empty() {
        "none".into()
    } else {
        p.level(Level::Warn, &found.join(", "))
    }
}

fn buffer_counts(b: &Buffer) -> [(u64, &'static str, &'static str); 3] {
    [
        (b.underruns, "underrun", "underruns"),
        (b.overruns, "overrun", "overruns"),
        (b.resyncs, "resync", "resyncs"),
    ]
}

fn render(s: &Status, p: Paint) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{}  {}",
        p.bold(&s.name),
        p.dim(&format!("{}  orpheusd {}", s.id, s.version))
    );
    if !s.audio {
        let mark = p.mark(Level::Bad, "no audio system, no streams");
        heading(&mut o, p, "Audio", Some(mark));
    }
    match &s.clock {
        Some(ClockInfo { ptp: Some(t), .. }) => clock(&mut o, p, t),
        Some(c) => {
            let source = match c.source.as_str() {
                "system" => "system clock",
                other => other,
            };
            let mark = p.mark(Level::Warn, source);
            heading(&mut o, p, "Clock", Some(mark));
        }
        None => {}
    }
    if let Some(r) = &s.receiver {
        receiver(&mut o, p, r);
    }
    if let Some(l) = &s.loopback {
        heading(&mut o, p, "Loopback", None);
        row(&mut o, p, "buffer", &format!("{} frames", l.level_frames));
        row(&mut o, p, "problems", &problems(p, &buffer_counts(l)));
    }
    senders(&mut o, p, &s.senders);
    o
}

fn clock(o: &mut String, p: Paint, t: &Ptp) {
    let mark = match t.state.as_str() {
        "faulty" | "disabled" => p.mark(Level::Bad, &format!("PTP, {}", t.state)),
        _ if t.settled => p.mark(Level::Good, "PTP, settled"),
        _ => p.mark(Level::Warn, "PTP, settling"),
    };
    heading(o, p, "Clock", Some(mark));
    let transport = match t.transport.as_str() {
        "ipv4" => "IPv4",
        "ipv6" => "IPv6",
        other => other,
    };
    row(o, p, "interface", &format!("{} ({transport})", t.interface));
    row(o, p, "identity", &t.identity);
    row(o, p, "state", &t.state.replace('_', " "));
    let gm = if t.grandmaster == t.identity {
        "this node"
    } else {
        &t.grandmaster
    };
    row(o, p, "grandmaster", gm);
    row(o, p, "offset", &us(t.offset_ns));
    row(o, p, "delay", &us(t.mean_delay_ns));
    row(o, p, "frequency", &format!("{:+.2} ppm", t.frequency_ppm));
    // One or two steps after a start are normal; more mean the clock jumps.
    let steps = t.steps.to_string();
    let steps = if t.steps > 2 {
        p.level(Level::Warn, &steps)
    } else {
        steps
    };
    row(o, p, "steps", &steps);
}

fn receiver(o: &mut String, p: Paint, r: &Receiver) {
    let mark = match r.ssrc {
        Some(ssrc) => p.mark(Level::Good, &format!("receiving ssrc {ssrc:08x}")),
        None => p.mark(Level::Off, "idle"),
    };
    heading(o, p, "Receiver", Some(mark));
    row(o, p, "listen", &r.listen);
    row(o, p, "link offset", &format!("{} ms", r.link_offset_ms));
    let margin = match r.margin_us {
        Some(m) => {
            let text = format!("{:.2} ms", m as f64 / 1000.0);
            match m {
                ..=0 => p.level(Level::Bad, &text),
                1..1000 => p.level(Level::Warn, &text),
                _ => text,
            }
        }
        None => "-".into(),
    };
    row(o, p, "margin", &margin);
    row(o, p, "packets", &r.packets.to_string());
    row(o, p, "buffer", &format!("{} frames", r.buffer.level_frames));
    row(o, p, "ASRC", &format!("{:+.1} ppm", r.asrc_ppm));
    let mut counts = vec![
        (r.lost, "lost", "lost"),
        (r.late, "late", "late"),
        (r.early, "early", "early"),
        (r.malformed, "malformed", "malformed"),
        (r.realigns, "realign", "realigns"),
    ];
    counts.extend(buffer_counts(&r.buffer));
    row(o, p, "problems", &problems(p, &counts));
}

fn senders(o: &mut String, p: Paint, senders: &[Sender]) {
    if senders.is_empty() {
        let mark = p.mark(Level::Off, "none");
        heading(o, p, "Senders", Some(mark));
        return;
    }
    heading(o, p, "Senders", None);
    let header = ["ID", "PEER", "PACKETS", "BUFFER", "ASRC", "PROBLEMS"];
    let rows: Vec<[String; 6]> = senders
        .iter()
        .map(|t| {
            let mut counts = vec![
                (t.send_errors, "send error", "send errors"),
                (t.realigns, "realign", "realigns"),
            ];
            counts.extend(buffer_counts(&t.buffer));
            [
                t.id.to_string(),
                t.peer.clone(),
                t.packets.to_string(),
                format!("{} frames", t.buffer.level_frames),
                format!("{:+.1} ppm", t.asrc_ppm),
                problems(p, &counts),
            ]
        })
        .collect();
    // Column widths from the plain text; the last column is not padded, so
    // its colour codes don't matter.
    let mut width = header.map(str::len);
    for r in &rows {
        for (w, cell) in width.iter_mut().zip(r) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let right = [false, false, true, true, true, false];
    let line = |cells: [&str; 6]| {
        let mut l = String::from(" ");
        for (i, cell) in cells.iter().enumerate() {
            if i == cells.len() - 1 {
                let _ = write!(l, " {cell}");
            } else if right[i] {
                let _ = write!(l, " {cell:>w$} ", w = width[i]);
            } else {
                let _ = write!(l, " {cell:<w$} ", w = width[i]);
            }
        }
        l
    };
    let _ = writeln!(o, "{}", p.dim(&line(header)));
    for r in &rows {
        let _ = writeln!(o, "{}", line(r.each_ref().map(String::as_str)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
        "id": "B73AQ0P0", "name": "desk", "version": "0.0.1", "audio": true,
        "clock": {"source": "ptp", "ptp": {
            "interface": "enp3s0", "transport": "ipv4",
            "identity": "4cb04a.fffe.50a582", "state": "slave",
            "grandmaster": "9c6b00.fffe.1a2b3c", "offset_ns": -412,
            "mean_delay_ns": 41200, "frequency_ppm": 12.31, "steps": 1,
            "settled": true}},
        "receiver": {"listen": "[::]:5004", "link_offset_ms": 10,
            "ssrc": 1579090602, "packets": 120000, "malformed": 0, "late": 3,
            "early": 0, "lost": 1, "realigns": 0, "margin_us": 8710,
            "asrc_ppm": -3.2, "buffer": {"underruns": 0, "overruns": 0,
            "resyncs": 0, "level_frames": 1140}},
        "senders": [{"id": 0, "peer": "192.168.1.20:5004", "packets": 120000,
            "send_errors": 0, "realigns": 0, "asrc_ppm": 8.9,
            "buffer": {"underruns": 2, "overruns": 0, "resyncs": 0,
            "level_frames": 1120}}]
    }"#;

    #[test]
    fn renders_plain_status() {
        let s: Status = serde_json::from_str(STATUS).unwrap();
        let out = render(&s, Paint(false));
        let expected = "\
desk  B73AQ0P0  orpheusd 0.0.1

Clock          ● PTP, settled
  interface    enp3s0 (IPv4)
  identity     4cb04a.fffe.50a582
  state        slave
  grandmaster  9c6b00.fffe.1a2b3c
  offset       -0.4 µs
  delay        41.2 µs
  frequency    +12.31 ppm
  steps        1

Receiver       ● receiving ssrc 5e1f02aa
  listen       [::]:5004
  link offset  10 ms
  margin       8.71 ms
  packets      120000
  buffer       1140 frames
  ASRC         -3.2 ppm
  problems     1 lost, 3 late

Senders
  ID  PEER               PACKETS       BUFFER      ASRC  PROBLEMS
  0   192.168.1.20:5004   120000  1120 frames  +8.9 ppm  2 underruns
";
        assert_eq!(out, expected, "\n{out}");
    }

    #[test]
    fn colour_stays_out_of_alignment() {
        let s: Status = serde_json::from_str(STATUS).unwrap();
        let plain = render(&s, Paint(false));
        let coloured = render(&s, Paint(true));
        let stripped = coloured
            .split("\x1b[")
            .enumerate()
            .map(|(i, part)| {
                if i == 0 {
                    part
                } else {
                    &part[part.find('m').unwrap() + 1..]
                }
            })
            .collect::<String>();
        assert_eq!(stripped, plain);
    }
}
