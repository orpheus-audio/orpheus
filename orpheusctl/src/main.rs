//! `orpheusctl`: command-line client for any Orpheus node.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Base URL of the node to talk to.
    #[arg(long, default_value = "http://localhost:7480", env = "ORPHEUS_NODE")]
    node: String,
    /// Print the node's JSON responses instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the node's ID, name, version and streams.
    Status {
        /// Refresh every SECONDS (default 1), with counters as rates.
        #[arg(long, value_name = "SECONDS", num_args = 0..=1, default_missing_value = "1")]
        watch: Option<f64>,
    },
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
    /// List the other nodes on the network.
    Nodes,
    /// Stop a sender started with `connect`.
    Disconnect {
        /// Sender ID, as shown by `status`.
        id: u32,
    },
}

#[derive(Clone, Deserialize)]
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

#[derive(Clone, Deserialize)]
struct ClockInfo {
    source: String,
    ptp: Option<Ptp>,
}

fn ipv4() -> String {
    "ipv4".into()
}

#[derive(Clone, Deserialize)]
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

#[derive(Clone, Deserialize)]
struct Buffer {
    underruns: u64,
    overruns: u64,
    resyncs: u64,
    level_frames: u64,
}

#[derive(Clone, Deserialize)]
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

#[derive(Clone, Deserialize)]
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

/// Another node, as found over mDNS.
#[derive(Clone, Deserialize)]
struct Node {
    id: Option<String>,
    name: String,
    version: Option<String>,
    #[serde(default)]
    addresses: Vec<String>,
    online: bool,
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

impl Status {
    /// The counters' increase since `prev`; levels and settings stay current.
    fn since(&self, prev: &Status) -> Status {
        let mut d = self.clone();
        d.loopback = match (&self.loopback, &prev.loopback) {
            (Some(b), Some(p)) => Some(b.since(p)),
            (b, _) => b.as_ref().map(|b| b.since(b)),
        };
        d.receiver = self.receiver.as_ref().map(|r| {
            let p = prev.receiver.as_ref().unwrap_or(r);
            Receiver {
                packets: r.packets.saturating_sub(p.packets),
                malformed: r.malformed.saturating_sub(p.malformed),
                late: r.late.saturating_sub(p.late),
                early: r.early.saturating_sub(p.early),
                lost: r.lost.saturating_sub(p.lost),
                realigns: r.realigns.saturating_sub(p.realigns),
                buffer: r.buffer.since(&p.buffer),
                ..r.clone()
            }
        });
        d.senders = self
            .senders
            .iter()
            .map(|t| {
                // A new sender counts from now.
                let p = prev.senders.iter().find(|p| p.id == t.id).unwrap_or(t);
                Sender {
                    packets: t.packets.saturating_sub(p.packets),
                    send_errors: t.send_errors.saturating_sub(p.send_errors),
                    realigns: t.realigns.saturating_sub(p.realigns),
                    buffer: t.buffer.since(&p.buffer),
                    ..t.clone()
                }
            })
            .collect();
        d
    }
}

impl Buffer {
    fn since(&self, prev: &Buffer) -> Buffer {
        Buffer {
            underruns: self.underruns.saturating_sub(prev.underruns),
            overruns: self.overruns.saturating_sub(prev.overruns),
            resyncs: self.resyncs.saturating_sub(prev.resyncs),
            level_frames: self.level_frames,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = cli.node.trim_end_matches('/');
    match cli.command {
        Command::Status { watch: None } => {
            let (raw, s) = status(base)?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                print!("{}", render(&s, Paint::detect(), Counts::Total));
            }
        }
        Command::Status { watch: Some(secs) } => {
            anyhow::ensure!(secs >= 0.1, "--watch needs at least 0.1 seconds");
            watch(base, Duration::from_secs_f64(secs), cli.json)?;
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
            if cli.json {
                println!("{}", serde_json::json!({ "id": created.id }));
            } else {
                println!("sender {} → {peer}", created.id);
            }
        }
        Command::Nodes => {
            let url = format!("{base}/x-orpheus/v1/nodes");
            let raw = ureq::get(&url)
                .call()
                .and_then(|mut r| r.body_mut().read_to_string())
                .with_context(|| format!("querying {url}"))?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let nodes: Vec<Node> = serde_json::from_str(&raw)
                    .with_context(|| format!("reading nodes from {url}"))?;
                print!("{}", render_nodes(&nodes, Paint::detect()));
            }
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

/// The status as the node sent it, and parsed.
fn status(base: &str) -> Result<(String, Status)> {
    let url = format!("{base}/x-orpheus/v1/status");
    let raw = ureq::get(&url)
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .with_context(|| format!("querying {url}"))?;
    let s = serde_json::from_str(&raw).with_context(|| format!("reading status from {url}"))?;
    Ok((raw, s))
}

/// Shows the status every `interval` until interrupted. On a terminal each
/// frame replaces the last; otherwise frames follow each other. With JSON
/// every sample is one line. Errors are shown and retried, so the node may
/// restart meanwhile.
fn watch(base: &str, interval: Duration, json: bool) -> Result<()> {
    let paint = Paint::detect();
    let tty = std::io::stdout().is_terminal();
    let mut out = std::io::stdout().lock();
    let mut prev: Option<(Instant, Status)> = None;
    let mut first = true;
    let mut next = Instant::now();
    loop {
        let fetched = status(base);
        let now = Instant::now();
        let frame = match fetched {
            Ok((raw, _)) if json => Some(format!("{}\n", raw.trim_end())),
            Ok((_, s)) => {
                // Rates need two samples; the first frame waits for the second.
                let frame = prev.as_ref().map(|(at, p)| {
                    let secs = (now - *at).as_secs_f64();
                    render(&s.since(p), paint, Counts::Rate(secs))
                });
                prev = Some((now, s));
                frame
            }
            Err(e) => {
                prev = None;
                let mark = paint.mark(Level::Bad, &format!("{e:#}"));
                Some(format!("{mark}\n"))
            }
        };
        if let Some(frame) = frame {
            if json {
                out.write_all(frame.as_bytes())?;
            } else if tty {
                // Home, then overwrite each line and clear the rest, which
                // flickers less than clearing the screen first.
                let mut f = String::from(if first { "\x1b[2J\x1b[H" } else { "\x1b[H" });
                for line in frame.lines() {
                    let _ = writeln!(f, "{line}\x1b[K");
                }
                f.push_str("\x1b[J");
                out.write_all(f.as_bytes())?;
            } else {
                if !first {
                    out.write_all(b"\n")?;
                }
                out.write_all(frame.as_bytes())?;
            }
            out.flush()?;
            first = false;
        }
        next += interval;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

/// How counters are shown.
#[derive(Clone, Copy)]
enum Counts {
    /// As they are: totals since the start.
    Total,
    /// Per second, from their increase over this many seconds.
    Rate(f64),
}

impl Counts {
    /// The number, and whether the name after it is singular.
    fn show(self, n: u64) -> (String, bool) {
        match self {
            Counts::Total => (n.to_string(), n == 1),
            Counts::Rate(secs) => {
                let r = n as f64 / secs;
                let text = if r >= 10.0 || r.fract() == 0.0 {
                    format!("{r:.0}")
                } else {
                    format!("{r:.1}")
                };
                (text, r == 1.0)
            }
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Counts::Total => "",
            Counts::Rate(_) => "/s",
        }
    }

    fn packets(self, n: u64) -> String {
        format!("{}{}", self.show(n).0, self.unit())
    }
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
fn problems(p: Paint, c: Counts, counts: &[(u64, &str, &str)]) -> String {
    let found: Vec<String> = counts
        .iter()
        .filter(|(n, ..)| *n > 0)
        .map(|&(n, one, many)| {
            let (num, single) = c.show(n);
            format!("{num} {}{}", if single { one } else { many }, c.unit())
        })
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

fn render(s: &Status, p: Paint, c: Counts) -> String {
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
        receiver(&mut o, p, c, r);
    }
    if let Some(l) = &s.loopback {
        heading(&mut o, p, "Loopback", None);
        row(&mut o, p, "buffer", &format!("{} frames", l.level_frames));
        row(&mut o, p, "problems", &problems(p, c, &buffer_counts(l)));
    }
    senders(&mut o, p, c, &s.senders);
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

fn receiver(o: &mut String, p: Paint, c: Counts, r: &Receiver) {
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
    row(o, p, "packets", &c.packets(r.packets));
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
    row(o, p, "problems", &problems(p, c, &counts));
}

fn senders(o: &mut String, p: Paint, c: Counts, senders: &[Sender]) {
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
                c.packets(t.packets),
                format!("{} frames", t.buffer.level_frames),
                format!("{:+.1} ppm", t.asrc_ppm),
                problems(p, c, &counts),
            ]
        })
        .collect();
    let right = [false, false, true, true, true, false];
    table(o, p, header, right, &rows);
}

/// A table with a dim header. Widths come from the plain text; the last
/// column is not padded, so it may carry colour codes.
fn table<const N: usize>(
    o: &mut String,
    p: Paint,
    header: [&str; N],
    right: [bool; N],
    rows: &[[String; N]],
) {
    let mut width = header.map(str::len);
    for r in rows {
        for (w, cell) in width.iter_mut().zip(r) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; N]| {
        let mut l = String::from(" ");
        for (i, cell) in cells.iter().enumerate() {
            if i == N - 1 {
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
    for r in rows {
        let _ = writeln!(o, "{}", line(r.each_ref().map(String::as_str)));
    }
}

/// The nodes a node found on the network.
fn render_nodes(nodes: &[Node], p: Paint) -> String {
    let mut o = String::new();
    if nodes.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, "no other nodes found"));
        return o;
    }
    let rows: Vec<[String; 5]> = nodes
        .iter()
        .map(|n| {
            let state = if n.online {
                p.mark(Level::Good, "online")
            } else {
                p.mark(Level::Off, "offline")
            };
            [
                n.name.clone(),
                n.id.clone().unwrap_or_else(|| "-".into()),
                n.addresses.first().cloned().unwrap_or_else(|| "-".into()),
                n.version.clone().unwrap_or_else(|| "-".into()),
                state,
            ]
        })
        .collect();
    let header = ["NAME", "ID", "ADDRESS", "VERSION", "STATE"];
    table(&mut o, p, header, [false; 5], &rows);
    o
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
        let out = render(&s, Paint(false), Counts::Total);
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
    fn watch_shows_rates() {
        let prev: Status = serde_json::from_str(STATUS).unwrap();
        let mut s = prev.clone();
        let r = s.receiver.as_mut().unwrap();
        r.packets += 2000;
        r.late += 3;
        s.senders[0].packets += 2000;
        s.senders[0].buffer.underruns += 2;
        let mut new = s.senders[0].clone();
        new.id = 1;
        s.senders.push(new);
        let out = render(&s.since(&prev), Paint(false), Counts::Rate(2.0));
        assert!(out.contains("  packets      1000/s\n"), "{out}");
        assert!(out.contains("  problems     1.5 late/s\n"), "{out}");
        assert!(
            out.contains("1000/s  1120 frames  +8.9 ppm  1 underrun/s\n"),
            "{out}"
        );
        // A sender that appeared since the last sample counts from now.
        assert!(
            out.contains("  1   192.168.1.20:5004      0/s  1120 frames  +8.9 ppm  none\n"),
            "{out}"
        );
    }

    #[test]
    fn colour_stays_out_of_alignment() {
        let s: Status = serde_json::from_str(STATUS).unwrap();
        let plain = render(&s, Paint(false), Counts::Total);
        let coloured = render(&s, Paint(true), Counts::Total);
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

    #[test]
    fn renders_nodes() {
        let nodes: Vec<Node> = serde_json::from_str(
            r#"[
                {"id":"B73AQ0P0","name":"Kitchen","version":"0.0.1","host":"orpheus-b73aq0p0.local.",
                 "addresses":["10.99.0.2","fe80::2%veth-a"],"port":7480,"online":true},
                {"name":"nmos-cpp node","host":"box.local.","addresses":[],"port":1080,"online":false}
            ]"#,
        )
        .unwrap();
        let expected = "  NAME           ID        ADDRESS    VERSION  STATE
  Kitchen        B73AQ0P0  10.99.0.2  0.0.1    ● online
  nmos-cpp node  -         -          -        ○ offline
";
        assert_eq!(render_nodes(&nodes, Paint(false)), expected);
        assert_eq!(render_nodes(&[], Paint(false)), "○ no other nodes found\n");
    }
}
