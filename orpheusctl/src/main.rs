//! `orpheusctl`: command-line client for any Orpheus node.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// The node to talk to: a host name or address (API on port 7480) or
    /// the URL of its API.
    #[arg(long, global = true, default_value = "localhost", env = "ORPHEUS_NODE")]
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
    /// Send the node's audio to a peer: creates a sink "<name>" whose
    /// audio is streamed to a free receiver of the peer, and connects that
    /// receiver to it.
    Connect {
        /// Peer: its name or ID (see `nodes`), host name or address, or the
        /// URL of its API. With --send-only: host name or address,
        /// optionally with :port (default 5004); IPv6 with a port as
        /// [addr]:port.
        peer: String,
        /// Name shown in the sink; defaults to the peer's name.
        #[arg(long)]
        name: Option<String>,
        /// Only send; don't connect a receiver on the peer (for receivers
        /// without an NMOS API, or set to accept any stream).
        #[arg(long)]
        send_only: bool,
        /// Take over the peer's receiver even if another sender is
        /// connected to it.
        #[arg(long)]
        force: bool,
        /// `high` plays over the peer's other streams and turns them down
        /// meanwhile, whatever the peer's policy (for announcements).
        #[arg(long, value_parser = ["normal", "high"], default_value = "normal")]
        priority: String,
    },
    /// List the other nodes on the network.
    Nodes,
    /// Give another node a name of your own, shown here instead of the
    /// one it announces: in `nodes`, in sinks to it and in streams from it.
    Alias {
        /// The node: its name, alias or ID (see `nodes`).
        node: String,
        /// The new alias.
        #[arg(required_unless_present = "remove", conflicts_with = "remove")]
        alias: Option<String>,
        /// Remove the alias and show the announced name again.
        #[arg(long)]
        remove: bool,
    },
    /// Keep another node listed while it is offline, also after a restart.
    Pin {
        /// The node: its name, alias or ID (see `nodes`).
        node: String,
    },
    /// Stop keeping a node listed while it is offline.
    Unpin {
        /// The node: its name, alias or ID (see `nodes`).
        node: String,
    },
    /// List the node's outputs and their types.
    Outputs,
    /// Set what kind of device an output is, which other nodes show.
    SetType {
        /// The output: its name or description, or a unique part of either.
        output: String,
        /// speaker, headphones, headset, tv, hifi, computer, portable,
        /// phone or generic; `auto` goes back to the detected type.
        #[arg(name = "TYPE")]
        kind: String,
    },
    /// Show the settings that can change while the node runs, and where
    /// each comes from: the default, config.toml, or `set`.
    Settings,
    /// Change settings, kept across restarts (config.toml is not touched).
    Set {
        /// SECTION.KEY=VALUE, e.g. receiver.policy=mix or ptp.domain=1.
        #[arg(required = true, value_name = "SECTION.KEY=VALUE")]
        settings: Vec<String>,
    },
    /// Undo `set`: go back to config.toml or the default.
    Reset {
        /// SECTION.KEY, or SECTION for all of its keys.
        #[arg(required = true, value_name = "SECTION[.KEY]")]
        keys: Vec<String>,
    },
    /// Stop a sender started with `connect` and disconnect the peer's
    /// receiver.
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
    /// `connected` or `any`: what a free receiver plays.
    accept: Option<String>,
    #[serde(default)]
    receivers: Vec<Receiver>,
    #[serde(default)]
    senders: Vec<Sender>,
    /// Missing from older nodes.
    #[serde(default)]
    warnings: Vec<Warning>,
}

/// A problem with a network path.
#[derive(Clone, Deserialize)]
struct Warning {
    message: String,
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

/// A receiver: one per connection, and a free one.
#[derive(Clone, Deserialize)]
struct Receiver {
    id: u32,
    port: u16,
    /// The connected sender's name or address.
    sender: Option<String>,
    /// Absent while free or off.
    stream: Option<Stream>,
    /// What the node's policy does to the stream: `silent`, `playing`,
    /// `muted` or `ducked`.
    state: Option<String>,
    /// `normal` or `high`.
    priority: Option<String>,
    /// The kind of output.
    output: Option<String>,
    /// Description of the output it plays on.
    plays_on: Option<String>,
}

/// A running receiver's stream.
#[derive(Clone, Deserialize)]
struct Stream {
    listen: String,
    link_offset_ms: u32,
    ssrc: Option<u32>,
    packets: u64,
    malformed: u64,
    /// Turned away by the connection's source or payload type.
    ignored: u64,
    late: u64,
    early: u64,
    lost: u64,
    realigns: u64,
    margin_us: Option<i64>,
    jitter_us: Option<i64>,
    asrc_ppm: f64,
    buffer: Buffer,
    /// Peak level over the last second; `None` for silence.
    peak_dbfs: Option<f32>,
}

/// An output of a node.
#[derive(Clone, Deserialize)]
struct Output {
    name: String,
    description: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "override")]
    overridden: Option<String>,
    default: bool,
    /// The receivers playing here.
    #[serde(default)]
    receivers: Vec<u32>,
}

#[derive(Clone, Deserialize)]
struct Sender {
    id: u32,
    /// Missing from older nodes.
    #[serde(default)]
    name: String,
    peer: String,
    #[serde(default = "yes")]
    enabled: bool,
    packets: u64,
    send_errors: u64,
    realigns: u64,
    #[serde(default)]
    asrc_ppm: f64,
    buffer: Buffer,
    /// `normal` or `high`.
    priority: Option<String>,
    /// The peer's receiver connected to it; absent if it only sends.
    receiver: Option<PeerLink>,
}

#[derive(Clone, Deserialize)]
struct PeerLink {
    state: String,
    /// `muted` or `ducked` by the peer's policy.
    policy: Option<String>,
}

/// Another node, as found over mDNS.
#[derive(Clone, Deserialize)]
struct Node {
    id: Option<String>,
    name: String,
    /// This node's own name for it.
    alias: Option<String>,
    #[serde(default)]
    pinned: bool,
    version: Option<String>,
    #[serde(default)]
    addresses: Vec<String>,
    /// `wired` or `wifi`, as the node announces it.
    link: Option<String>,
    online: bool,
}

#[derive(Serialize)]
struct NewSender {
    peer: String,
    name: Option<String>,
    send_only: bool,
    force: bool,
    priority: String,
}

#[derive(Deserialize)]
struct Created {
    id: u32,
    /// Missing from older nodes.
    name: Option<String>,
    destination: Option<String>,
    receiver: Option<String>,
}

impl Status {
    /// The counters' increase since `prev`; levels and settings stay current.
    fn since(&self, prev: &Status) -> Status {
        let mut d = self.clone();
        d.loopback = match (&self.loopback, &prev.loopback) {
            (Some(b), Some(p)) => Some(b.since(p)),
            (b, _) => b.as_ref().map(|b| b.since(b)),
        };
        d.receivers = self
            .receivers
            .iter()
            .map(|r| {
                // A new stream counts from now.
                let p = prev
                    .receivers
                    .iter()
                    .find(|p| p.id == r.id)
                    .and_then(|p| p.stream.as_ref());
                let stream = r.stream.as_ref().map(|t| {
                    let p = p.unwrap_or(t);
                    Stream {
                        packets: t.packets.saturating_sub(p.packets),
                        malformed: t.malformed.saturating_sub(p.malformed),
                        ignored: t.ignored.saturating_sub(p.ignored),
                        late: t.late.saturating_sub(p.late),
                        early: t.early.saturating_sub(p.early),
                        lost: t.lost.saturating_sub(p.lost),
                        realigns: t.realigns.saturating_sub(p.realigns),
                        buffer: t.buffer.since(&p.buffer),
                        ..t.clone()
                    }
                });
                Receiver {
                    stream,
                    ..r.clone()
                }
            })
            .collect();
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let base = node_url(&cli.node);
    match run(cli, &base) {
        Ok(()) => ExitCode::SUCCESS,
        // The reader went away, as with `| head`.
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprint!("{}", render_error(&e, &base, Paint::detect_stderr()));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli, base: &str) -> Result<()> {
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
        Command::Connect {
            peer,
            name,
            send_only,
            force,
            priority,
        } => {
            let url = format!("{base}/x-orpheus/v1/senders");
            let raw = post(
                &url,
                NewSender {
                    peer: peer.clone(),
                    name,
                    send_only,
                    force,
                    priority,
                },
            )
            .with_context(|| format!("connecting to {peer} via {url}"))?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let c: Created = serde_json::from_str(&raw)
                    .with_context(|| format!("reading the reply from {url}"))?;
                let name = c.name.as_deref().unwrap_or(&peer);
                let to = c.destination.as_deref().unwrap_or(&peer);
                let how = if c.receiver.is_some() {
                    "receiver connected"
                } else {
                    "send only"
                };
                println!("sender {} \"{name}\" → {to} ({how})", c.id);
            }
        }
        Command::Nodes => {
            let url = format!("{base}/x-orpheus/v1/nodes");
            let raw = get(&url)?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let nodes: Vec<Node> = serde_json::from_str(&raw)
                    .with_context(|| format!("reading nodes from {url}"))?;
                print!("{}", render_nodes(&nodes, Paint::detect()));
            }
        }
        Command::Outputs => {
            let raw = get(&format!("{base}/x-orpheus/v1/outputs"))?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let outputs: Vec<Output> =
                    serde_json::from_str(&raw).context("reading the outputs")?;
                print!("{}", render_outputs(&outputs, Paint::detect()));
            }
        }
        Command::SetType { output, kind } => {
            let outputs: Vec<Output> =
                serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/outputs"))?)
                    .context("reading the outputs")?;
            let o = find_output(&outputs, &output)?;
            let url = format!("{base}/x-orpheus/v1/outputs/{}", encode(&o.name));
            if kind == "auto" {
                let r = agent().delete(&url).call()?;
                // Nothing to reset.
                if r.status() != 404 {
                    reply(r).with_context(|| format!("resetting via {url}"))?;
                }
            } else {
                let r = agent()
                    .put(&url)
                    .send_json(serde_json::json!({ "type": kind }))?;
                reply(r).with_context(|| format!("setting {kind} via {url}"))?;
            }
        }
        Command::Alias {
            node,
            alias,
            remove,
        } => {
            let id = find_node(base, &node)?;
            let alias = if remove { None } else { alias };
            change_node(base, &id, serde_json::json!({ "alias": alias }))?;
        }
        Command::Pin { node } => {
            let id = find_node(base, &node)?;
            change_node(base, &id, serde_json::json!({ "pinned": true }))?;
        }
        Command::Unpin { node } => {
            let id = find_node(base, &node)?;
            change_node(base, &id, serde_json::json!({ "pinned": false }))?;
        }
        Command::Settings => {
            let raw = get(&format!("{base}/x-orpheus/v1/settings"))?;
            print_settings(&raw, cli.json)?;
        }
        Command::Set { settings } => {
            let mut patch = serde_json::Map::new();
            for s in &settings {
                let (path, value) = s
                    .split_once('=')
                    .with_context(|| format!("{s}: expected SECTION.KEY=VALUE"))?;
                let (section, key) = path
                    .split_once('.')
                    .with_context(|| format!("{path}: expected SECTION.KEY"))?;
                // Numbers and booleans as such, anything else as a string.
                let value = match serde_json::from_str::<serde_json::Value>(value) {
                    Ok(v @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_))) => v,
                    _ => serde_json::Value::String(value.to_owned()),
                };
                section_of(&mut patch, section).insert(key.to_owned(), value);
            }
            let raw = change_settings(base, patch.into())?;
            print_settings(&raw, cli.json)?;
        }
        Command::Reset { keys } => {
            let mut patch = serde_json::Map::new();
            for k in &keys {
                match k.split_once('.') {
                    Some((section, key)) => {
                        section_of(&mut patch, section)
                            .insert(key.to_owned(), serde_json::Value::Null);
                    }
                    None => {
                        patch.insert(k.clone(), serde_json::Value::Null);
                    }
                }
            }
            let raw = change_settings(base, patch.into())?;
            print_settings(&raw, cli.json)?;
        }
        Command::Disconnect { id } => {
            let url = format!("{base}/x-orpheus/v1/senders/{id}");
            reply(agent().delete(&url).call()?)
                .with_context(|| format!("stopping sender {id} via {url}"))?;
        }
    }
    Ok(())
}

/// The device ID of the Orpheus node `query` names: by ID, alias or name.
fn find_node(base: &str, query: &str) -> Result<String> {
    let nodes: Vec<Node> = serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/nodes"))?)
        .context("reading the nodes")?;
    let is = |s: &Option<String>| s.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(query));
    let found = nodes
        .iter()
        .find(|n| is(&n.id))
        .or_else(|| nodes.iter().find(|n| is(&n.alias)))
        .or_else(|| nodes.iter().find(|n| n.name.eq_ignore_ascii_case(query)))
        .with_context(|| format!("no node {query:?}; see `orpheusctl nodes`"))?;
    found
        .id
        .clone()
        .with_context(|| format!("{query} is not an Orpheus node"))
}

/// Changes another node's alias or pin.
fn change_node(base: &str, id: &str, body: serde_json::Value) -> Result<()> {
    let url = format!("{base}/x-orpheus/v1/nodes/{id}");
    let r = agent()
        .patch(&url)
        .send_json(body)
        .with_context(|| format!("changing node {id} via {url}"))?;
    reply(r).map(drop)
}

/// The object for `section` in a settings patch.
fn section_of<'a>(
    patch: &'a mut serde_json::Map<String, serde_json::Value>,
    section: &str,
) -> &'a mut serde_json::Map<String, serde_json::Value> {
    patch
        .entry(section)
        .or_insert_with(|| serde_json::Value::Object(Default::default()))
        .as_object_mut()
        .expect("sections are objects")
}

/// PATCHes the node's settings; returns the settings now in effect.
fn change_settings(base: &str, patch: serde_json::Value) -> Result<String> {
    let url = format!("{base}/x-orpheus/v1/settings");
    let r = agent()
        .patch(&url)
        .content_type("application/merge-patch+json")
        .send(patch.to_string())
        .with_context(|| format!("changing settings via {url}"))?;
    reply(r)
}

fn print_settings(raw: &str, json: bool) -> Result<()> {
    if json {
        println!("{}", raw.trim_end());
    } else {
        let v: serde_json::Value = serde_json::from_str(raw).context("reading the settings")?;
        print!("{}", render_settings(&v, Paint::detect()));
    }
    Ok(())
}

/// The body of a reply; an error status becomes a [`Refused`] error.
fn reply(mut r: ureq::http::Response<ureq::Body>) -> Result<String> {
    let text = r.body_mut().read_to_string()?;
    if r.status().is_success() {
        return Ok(text);
    }
    // Orpheus answers in plain text, NMOS in JSON with an "error" field.
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| text.trim().to_owned());
    Err(Refused {
        status: r.status().as_u16(),
        message,
    }
    .into())
}

/// The node answered, but with an error status.
#[derive(Debug)]
struct Refused {
    status: u16,
    message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the node refused: {} (HTTP {})",
            self.message, self.status
        )
    }
}

impl std::error::Error for Refused {}

/// Why the node at `base` could not be reached, if that is what `e` means.
fn unreachable(e: &anyhow::Error) -> Option<String> {
    use std::io::ErrorKind;
    e.chain().find_map(|c| {
        let io = match c.downcast_ref::<ureq::Error>()? {
            ureq::Error::Io(io) => io,
            ureq::Error::HostNotFound => return Some("host not found".into()),
            ureq::Error::ConnectionFailed => return Some("connection failed".into()),
            ureq::Error::Timeout(_) => return Some("timed out".into()),
            _ => return None,
        };
        let reason = match io.kind() {
            ErrorKind::ConnectionRefused => "connection refused",
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => "connection reset",
            ErrorKind::HostUnreachable => "host unreachable",
            ErrorKind::NetworkUnreachable => "network unreachable",
            ErrorKind::TimedOut => "timed out",
            ErrorKind::AddrNotAvailable => "address not available",
            // Name resolution has no kind of its own.
            _ if io.to_string().contains("lookup address") => "host not found",
            _ => return None,
        };
        Some(reason.into())
    })
}

/// The API URL for `--node`: a URL as it is, a bare host on port 7480.
fn node_url(node: &str) -> String {
    let node = node.trim_end_matches('/');
    if node.contains("://") {
        return node.to_owned();
    }
    if node.parse::<std::net::Ipv6Addr>().is_ok() {
        return format!("http://[{node}]:7480");
    }
    // A port follows the last colon, after the brackets of an IPv6 address.
    let has_port = node
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    if has_port {
        format!("http://{node}")
    } else {
        format!("http://{node}:7480")
    }
}

/// Whether `base` points at this machine.
fn is_local(base: &str) -> bool {
    let Some(host) = base
        .parse::<ureq::http::Uri>()
        .ok()
        .and_then(|u| u.host().map(str::to_owned))
    else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// An error as `orpheusctl` shows it: a red dot, the message and, for a node
/// that cannot be reached, what to check.
fn render_error(e: &anyhow::Error, base: &str, p: Paint) -> String {
    match unreachable(e) {
        // Nothing listens here, so orpheusd is not running.
        Some(reason) if reason == "connection refused" && is_local(base) => format!(
            "{}\n  {}\n",
            p.mark(Level::Bad, "orpheusd is not running on this machine"),
            p.dim("Start it with orpheusd, or choose a node with --node or ORPHEUS_NODE."),
        ),
        Some(reason) => format!(
            "{}\n  {}\n",
            p.mark(Level::Bad, &format!("no Orpheus node at {base}: {reason}")),
            p.dim("Is orpheusd running there? Choose a node with --node or ORPHEUS_NODE."),
        ),
        None => format!("{}\n", p.mark(Level::Bad, &format!("{e:#}"))),
    }
}

/// An agent that leaves error statuses to [`reply`] and gives up quickly on
/// a node that does not answer.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(3)))
        .build()
        .new_agent()
}

/// GETs a body as text.
fn get(url: &str) -> Result<String> {
    agent()
        .get(url)
        .call()
        .map_err(anyhow::Error::from)
        .and_then(reply)
        .with_context(|| format!("querying {url}"))
}

/// The output `query` names: by name, by description, or by a part of
/// either that only one has.
fn find_output<'a>(outputs: &'a [Output], query: &str) -> Result<&'a Output> {
    let q = query.to_lowercase();
    if let Some(o) = outputs
        .iter()
        .find(|o| o.name == query || o.description.to_lowercase() == q)
    {
        return Ok(o);
    }
    let found: Vec<&Output> = outputs
        .iter()
        .filter(|o| o.name.to_lowercase().contains(&q) || o.description.to_lowercase().contains(&q))
        .collect();
    match found[..] {
        [o] => Ok(o),
        [] => anyhow::bail!("no output matches {query:?}; see `orpheusctl outputs`"),
        _ => anyhow::bail!(
            "{query:?} matches {}; be more specific",
            found
                .iter()
                .map(|o| format!("{:?}", o.description))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Percent-encodes a path segment.
fn encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// POSTs JSON; an error status becomes an error with the node's message.
fn post(url: &str, body: impl Serialize) -> Result<String> {
    let r = agent().post(url).send_json(body)?;
    reply(r)
}

/// The status as the node sent it, and parsed.
fn status(base: &str) -> Result<(String, Status)> {
    let url = format!("{base}/x-orpheus/v1/status");
    let raw = get(&url)?;
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
                Some(render_error(&e, base, paint))
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

/// ANSI styling, on only when the output is a terminal and `NO_COLOR` is unset.
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
        Self(std::io::stdout().is_terminal() && !Self::no_color())
    }

    /// For messages on stderr.
    fn detect_stderr() -> Self {
        Self(std::io::stderr().is_terminal() && !Self::no_color())
    }

    fn no_color() -> bool {
        std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
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
    if !s.warnings.is_empty() {
        heading(&mut o, p, "Warnings", None);
        for w in &s.warnings {
            let _ = writeln!(o, "  {}", p.mark(Level::Warn, &w.message));
        }
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
    for r in &s.receivers {
        receiver(&mut o, p, c, r, s.accept.as_deref());
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

fn receiver(o: &mut String, p: Paint, c: Counts, r: &Receiver, accept: Option<&str>) {
    let title = format!("Receiver {}", r.id);
    let Some(st) = &r.stream else {
        let mark = match &r.sender {
            Some(from) => p.mark(Level::Off, &format!("connected to {from}, off")),
            None => p.mark(Level::Off, &format!("free, port {}", r.port)),
        };
        heading(o, p, &title, Some(mark));
        return;
    };
    let mark = match (st.ssrc, &r.sender) {
        (Some(_), Some(from)) => match r.state.as_deref() {
            Some("muted") => p.mark(Level::Off, &format!("receiving from {from}, muted")),
            Some("ducked") => p.mark(Level::Good, &format!("receiving from {from}, ducked")),
            _ => p.mark(Level::Good, &format!("receiving from {from}")),
        },
        (Some(ssrc), None) => p.mark(Level::Good, &format!("receiving ssrc {ssrc:08x}")),
        (None, Some(from)) => p.mark(Level::Off, &format!("connected to {from}, idle")),
        (None, None) if accept == Some("any") => p.mark(Level::Off, "free, plays any sender"),
        (None, None) => p.mark(Level::Off, "idle"),
    };
    heading(o, p, &title, Some(mark));
    row(o, p, "listen", &st.listen);
    if let Some(output) = &r.output {
        let output = match &r.plays_on {
            Some(on) => format!("{output} ({on})"),
            None => output.clone(),
        };
        row(o, p, "output", &output);
    }
    if r.priority.as_deref() == Some("high") {
        row(o, p, "priority", "high");
    }
    row(o, p, "link offset", &format!("{} ms", st.link_offset_ms));
    let level = st
        .peak_dbfs
        .map_or("silence".into(), |db| format!("{db:.1} dBFS"));
    row(o, p, "level", &level);
    let margin = match st.margin_us {
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
    if let Some(j) = st.jitter_us {
        let text = format!("{:.2} ms", j as f64 / 1000.0);
        // As orpheusd's health check warns.
        let text = if j > 3000 {
            p.level(Level::Warn, &text)
        } else {
            text
        };
        row(o, p, "jitter", &text);
    }
    row(o, p, "packets", &c.packets(st.packets));
    if st.ignored > 0 {
        // From other sources: expected while connected to one sender.
        row(o, p, "ignored", &c.packets(st.ignored));
    }
    row(
        o,
        p,
        "buffer",
        &format!("{} frames", st.buffer.level_frames),
    );
    row(o, p, "ASRC", &format!("{:+.1} ppm", st.asrc_ppm));
    let mut counts = vec![
        (st.lost, "lost", "lost"),
        (st.late, "late", "late"),
        (st.early, "early", "early"),
        (st.malformed, "malformed", "malformed"),
        (st.realigns, "realign", "realigns"),
    ];
    counts.extend(buffer_counts(&st.buffer));
    row(o, p, "problems", &problems(p, c, &counts));
}

fn senders(o: &mut String, p: Paint, c: Counts, senders: &[Sender]) {
    if senders.is_empty() {
        let mark = p.mark(Level::Off, "none");
        heading(o, p, "Senders", Some(mark));
        return;
    }
    heading(o, p, "Senders", None);
    let header = [
        "ID", "NAME", "PEER", "PACKETS", "BUFFER", "ASRC", "PROBLEMS",
    ];
    let rows: Vec<[String; 7]> = senders
        .iter()
        .map(|t| {
            let mut counts = vec![
                (t.send_errors, "send error", "send errors"),
                (t.realigns, "realign", "realigns"),
            ];
            counts.extend(buffer_counts(&t.buffer));
            [
                t.id.to_string(),
                t.name.clone(),
                peer_column(t),
                c.packets(t.packets),
                format!("{} frames", t.buffer.level_frames),
                format!("{:+.1} ppm", t.asrc_ppm),
                problems(p, c, &counts),
            ]
        })
        .collect();
    let right = [false, false, false, true, true, true, false];
    table(o, p, header, right, &rows);
}

/// Where a sender sends, and how that stands.
fn peer_column(t: &Sender) -> String {
    let mut notes = vec![];
    match t.receiver.as_ref().map(|r| r.state.as_str()) {
        Some("disconnected") => notes.push("disconnected, paused".to_owned()),
        Some(s @ ("unreachable" | "connecting")) => notes.push(s.to_owned()),
        _ if !t.enabled => notes.push("off".into()),
        _ => {}
    }
    match t.receiver.as_ref().and_then(|r| r.policy.as_deref()) {
        Some("muted") => notes.push("muted by policy".into()),
        Some(held) => notes.push(held.to_owned()),
        None => {}
    }
    if t.priority.as_deref() == Some("high") {
        notes.push("high priority".into());
    }
    if notes.is_empty() {
        t.peer.clone()
    } else {
        format!("{} ({})", t.peer, notes.join(", "))
    }
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
    let _ = writeln!(o, "{}", p.dim(line(header).trim_end()));
    for r in rows {
        let _ = writeln!(o, "{}", line(r.each_ref().map(String::as_str)).trim_end());
    }
}

/// A node's outputs.
fn render_outputs(outputs: &[Output], p: Paint) -> String {
    let mut o = String::new();
    if outputs.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, "no outputs"));
        return o;
    }
    let rows: Vec<[String; 4]> = outputs
        .iter()
        .map(|out| {
            let kind = match &out.overridden {
                Some(_) => format!("{} (set)", out.kind),
                None => out.kind.clone(),
            };
            let mut notes = Vec::new();
            if !out.receivers.is_empty() {
                let ids: Vec<String> = out.receivers.iter().map(u32::to_string).collect();
                let noun = if ids.len() == 1 {
                    "receiver"
                } else {
                    "receivers"
                };
                notes.push(format!("{noun} {}", ids.join(", ")));
            }
            if out.default {
                notes.push("default".into());
            }
            [
                out.description.clone(),
                kind,
                out.name.clone(),
                notes.join(", "),
            ]
        })
        .collect();
    let header = ["OUTPUT", "TYPE", "NAME", ""];
    table(&mut o, p, header, [false; 4], &rows);
    o
}

/// The settings, one per line, with where each comes from.
fn render_settings(v: &serde_json::Value, p: Paint) -> String {
    let mut rows = Vec::new();
    let sections = v.as_object().into_iter().flatten();
    for (section, keys) in sections.filter(|(s, _)| *s != "sources") {
        for (key, value) in keys.as_object().into_iter().flatten() {
            let value = match value {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => "-".into(),
                other => other.to_string(),
            };
            let from = v["sources"][section][key].as_str().unwrap_or("default");
            let from = if from == "default" {
                p.dim(from)
            } else {
                from.to_owned()
            };
            rows.push([format!("{section}.{key}"), value, from]);
        }
    }
    let mut o = String::new();
    table(&mut o, p, ["SETTING", "VALUE", "FROM"], [false; 3], &rows);
    o
}

/// The nodes a node found on the network.
fn render_nodes(nodes: &[Node], p: Paint) -> String {
    let mut o = String::new();
    if nodes.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, "no other nodes found"));
        return o;
    }
    let rows: Vec<[String; 6]> = nodes
        .iter()
        .map(|n| {
            let pin = if n.pinned { ", pinned" } else { "" };
            let state = if n.online {
                p.mark(Level::Good, &format!("online{pin}"))
            } else {
                p.mark(Level::Off, &format!("offline{pin}"))
            };
            [
                match &n.alias {
                    Some(a) => format!("{a} ({})", n.name),
                    None => n.name.clone(),
                },
                n.id.clone().unwrap_or_else(|| "-".into()),
                n.addresses.first().cloned().unwrap_or_else(|| "-".into()),
                n.version.clone().unwrap_or_else(|| "-".into()),
                match n.link.as_deref() {
                    Some("wired") => "wired".into(),
                    Some("wifi") => p.level(Level::Warn, "Wi-Fi"),
                    _ => "-".into(),
                },
                state,
            ]
        })
        .collect();
    let header = ["NAME", "ID", "ADDRESS", "VERSION", "LINK", "STATE"];
    table(&mut o, p, header, [false; 6], &rows);
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
        "accept": "connected",
        "receivers": [{"id": 0, "port": 5004, "stream": {"listen": "[::]:5004",
            "link_offset_ms": 10, "ssrc": 1579090602, "packets": 120000,
            "malformed": 0, "ignored": 0, "late": 3, "early": 0, "lost": 1,
            "realigns": 0, "margin_us": 8710, "jitter_us": null, "asrc_ppm": -3.2,
            "buffer": {"underruns": 0, "overruns": 0, "resyncs": 0,
            "level_frames": 1140}, "peak_dbfs": -12.04}},
            {"id": 1, "port": 5006}],
        "senders": [{"id": 0, "name": "Kitchen", "peer": "192.168.1.20:5004", "packets": 120000,
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

Receiver 0     ● receiving ssrc 5e1f02aa
  listen       [::]:5004
  link offset  10 ms
  level        -12.0 dBFS
  margin       8.71 ms
  packets      120000
  buffer       1140 frames
  ASRC         -3.2 ppm
  problems     1 lost, 3 late

Receiver 1     ○ free, port 5006

Senders
  ID  NAME     PEER               PACKETS       BUFFER      ASRC  PROBLEMS
  0   Kitchen  192.168.1.20:5004   120000  1120 frames  +8.9 ppm  2 underruns
";
        assert_eq!(out, expected, "\n{out}");
    }

    #[test]
    fn watch_shows_rates() {
        let prev: Status = serde_json::from_str(STATUS).unwrap();
        let mut s = prev.clone();
        let r = s.receivers[0].stream.as_mut().unwrap();
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
            out.contains(
                "  1   Kitchen  192.168.1.20:5004      0/s  1120 frames  +8.9 ppm  none\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn shows_ports_that_are_off() {
        let mut s: Status = serde_json::from_str(STATUS).unwrap();
        s.receivers[0].stream.as_mut().unwrap().ignored = 42;
        s.receivers[1].sender = Some("Laptop".into());
        s.senders[0].enabled = false;
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("Receiver 1     ○ connected to Laptop, off\n"),
            "{out}"
        );
        assert!(out.contains("  ignored      42\n"), "{out}");
        assert!(
            out.contains("  Kitchen  192.168.1.20:5004 (off)  "),
            "{out}"
        );

        s.senders[0].receiver = Some(PeerLink {
            state: "disconnected".into(),
            policy: None,
        });
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("  Kitchen  192.168.1.20:5004 (disconnected, paused)  "),
            "{out}"
        );
    }

    #[test]
    fn shows_policy_and_priority() {
        let mut s: Status = serde_json::from_str(STATUS).unwrap();
        let r = &mut s.receivers[0];
        r.sender = Some("Laptop".into());
        r.state = Some("muted".into());
        r.priority = Some("high".into());
        s.senders[0].receiver = Some(PeerLink {
            state: "connected".into(),
            policy: Some("muted".into()),
        });
        s.senders[0].priority = Some("high".into());
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("Receiver 0     ○ receiving from Laptop, muted\n"),
            "{out}"
        );
        assert!(out.contains("  priority     high\n"), "{out}");
        assert!(
            out.contains("  Kitchen  192.168.1.20:5004 (muted by policy, high priority)  "),
            "{out}"
        );
        s.receivers[0].state = Some("ducked".into());
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("Receiver 0     ● receiving from Laptop, ducked\n"),
            "{out}"
        );
    }

    #[test]
    fn shows_connections() {
        let mut s: Status = serde_json::from_str(STATUS).unwrap();
        let r = &mut s.receivers[0];
        r.output = Some("speaker".into());
        r.plays_on = Some("Built-in Audio".into());
        r.sender = Some("Laptop".into());
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("Receiver 0     ● receiving from Laptop\n"),
            "{out}"
        );
        assert!(
            out.contains("  output       speaker (Built-in Audio)\n"),
            "{out}"
        );
        let r = s.receivers[0].stream.as_mut().unwrap();
        r.ssrc = None;
        r.peak_dbfs = None;
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("Receiver 0     ○ connected to Laptop, idle\n"),
            "{out}"
        );
        assert!(out.contains("  level        silence\n"), "{out}");
        s.receivers[0].sender = None;
        s.accept = Some("any".into());
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("Receiver 0     ○ free, plays any sender\n"),
            "{out}"
        );
    }

    #[test]
    fn shows_network_warnings() {
        let mut s: Status = serde_json::from_str(STATUS).unwrap();
        s.warnings.push(Warning {
            message: "sender 0: Kitchen is on Wi-Fi; expect dropouts".into(),
        });
        s.receivers[0].stream.as_mut().unwrap().jitter_us = Some(4210);
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("\nWarnings\n  ● sender 0: Kitchen is on Wi-Fi; expect dropouts\n"),
            "{out}"
        );
        assert!(out.contains("  jitter       4.21 ms\n"), "{out}");

        let nodes: Vec<Node> = serde_json::from_str(
            r#"[{"name": "Kitchen", "addresses": ["10.0.0.2"], "link": "wifi", "online": true},
                {"name": "Desk", "addresses": ["10.0.0.3"], "online": true}]"#,
        )
        .unwrap();
        let out = render_nodes(&nodes, Paint(false));
        assert!(out.contains("10.0.0.2  -        Wi-Fi  ● online"), "{out}");
        assert!(out.contains("10.0.0.3  -        -      ● online"), "{out}");
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
    fn outputs() {
        let outputs: Vec<Output> = serde_json::from_str(
            r#"[
                {"name":"alsa_output.pci-0000_00_1f.3.analog-stereo","description":"Built-in Audio",
                 "type":"hifi","detected":"computer","override":"hifi","default":true,"receivers":[0,2]},
                {"name":"orpheus.send.0","description":"Kitchen","type":"speaker",
                 "detected":"speaker","default":false,"receivers":[]}
            ]"#,
        )
        .unwrap();
        let expected = "  OUTPUT          TYPE        NAME\n  Built-in Audio  hifi (set)  alsa_output.pci-0000_00_1f.3.analog-stereo  receivers 0, 2, default\n  Kitchen         speaker     orpheus.send.0\n";
        assert_eq!(render_outputs(&outputs, Paint(false)), expected);
        assert_eq!(
            find_output(&outputs, "kitchen").unwrap().name,
            "orpheus.send.0"
        );
        assert_eq!(
            find_output(&outputs, "pci").unwrap().description,
            "Built-in Audio"
        );
        assert!(find_output(&outputs, "i").is_err());
        assert!(find_output(&outputs, "nope").is_err());
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn renders_nodes() {
        let nodes: Vec<Node> = serde_json::from_str(
            r#"[
                {"id":"B73AQ0P0","name":"Kitchen","version":"0.0.1","host":"orpheus-b73aq0p0.local.",
                 "addresses":["10.99.0.2","fe80::2%veth-a"],"port":7480,"online":true},
                {"id":"C73AQ0P0","name":"Bad","alias":"Bathroom","version":"0.0.1","host":"orpheus-c73aq0p0.local.",
                 "addresses":["10.99.0.3"],"port":7480,"online":false,"pinned":true},
                {"name":"nmos-cpp node","host":"box.local.","addresses":[],"port":1080,"online":false}
            ]"#,
        )
        .unwrap();
        let expected = "  NAME            ID        ADDRESS    VERSION  LINK  STATE
  Kitchen         B73AQ0P0  10.99.0.2  0.0.1    -     ● online
  Bathroom (Bad)  C73AQ0P0  10.99.0.3  0.0.1    -     ○ offline, pinned
  nmos-cpp node   -         -          -        -     ○ offline
";
        assert_eq!(render_nodes(&nodes, Paint(false)), expected);
        assert_eq!(render_nodes(&[], Paint(false)), "○ no other nodes found\n");
    }

    #[test]
    fn node_urls() {
        assert_eq!(node_url("localhost"), "http://localhost:7480");
        assert_eq!(node_url("kitchen.local:7481"), "http://kitchen.local:7481");
        assert_eq!(node_url("192.168.1.20"), "http://192.168.1.20:7480");
        assert_eq!(node_url("fd00::20"), "http://[fd00::20]:7480");
        assert_eq!(node_url("[fd00::20]"), "http://[fd00::20]:7480");
        assert_eq!(node_url("[fd00::20]:7481"), "http://[fd00::20]:7481");
        assert_eq!(node_url("https://node.example/"), "https://node.example");
    }

    #[test]
    fn local() {
        assert!(is_local("http://localhost:7480"));
        assert!(is_local("http://127.0.0.1:7480"));
        assert!(is_local("http://[::1]:7480"));
        assert!(!is_local("http://kitchen.local:7480"));
        assert!(!is_local("http://[fd00::20]:7480"));
    }

    #[test]
    fn errors() {
        let base = "http://localhost:7480";
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let e = anyhow::Error::from(ureq::Error::Io(refused)).context("querying");
        assert_eq!(
            render_error(&e, base, Paint(false)),
            "● orpheusd is not running on this machine
  Start it with orpheusd, or choose a node with --node or ORPHEUS_NODE.
"
        );
        assert_eq!(
            render_error(&e, "http://kitchen.local:7480", Paint(false)),
            "● no Orpheus node at http://kitchen.local:7480: connection refused
  Is orpheusd running there? Choose a node with --node or ORPHEUS_NODE.
"
        );
        let e = anyhow::Error::from(Refused {
            status: 400,
            message: "empty alias".into(),
        });
        assert_eq!(
            render_error(&e, base, Paint(false)),
            "● the node refused: empty alias (HTTP 400)\n"
        );
    }
}
