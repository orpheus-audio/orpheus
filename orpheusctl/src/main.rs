//! `orpheusctl`: command-line client for any Orpheus node.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{FromArgMatches, Parser, Subcommand};
use orpheus_core::Layout;
use serde::{Deserialize, Serialize};

use crate::i18n::{num, t};

mod i18n;

// The help is in `locales/*/orpheusctl.ftl`; see `i18n::command`.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(long, global = true, default_value = "localhost", env = "ORPHEUS_NODE")]
    node: String,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status {
        #[arg(long, value_name = "SECONDS", num_args = 0..=1, default_missing_value = "1")]
        watch: Option<f64>,
    },
    Connect {
        peer: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        send_only: bool,
        #[arg(long)]
        force: bool,
        #[arg(long, value_parser = ["normal", "high"], default_value = "normal")]
        priority: String,
        #[arg(long)]
        layout: Option<String>,
    },
    Nodes,
    Alias {
        node: String,
        #[arg(required_unless_present = "remove", conflicts_with = "remove")]
        alias: Option<String>,
        #[arg(long)]
        remove: bool,
    },
    Pin {
        node: String,
    },
    Unpin {
        node: String,
    },
    Outputs,
    Inputs,
    Output {
        #[command(subcommand)]
        action: OutputAction,
    },
    SetType {
        output: String,
        #[arg(name = "TYPE")]
        kind: String,
    },
    Play {
        receiver: u32,
        output: String,
    },
    Default {
        output: String,
    },
    Settings,
    Set {
        #[arg(required = true, value_name = "SECTION.KEY=VALUE")]
        settings: Vec<String>,
    },
    Reset {
        #[arg(required = true, value_name = "SECTION[.KEY]")]
        keys: Vec<String>,
    },
    Routes,
    Disconnect {
        route: String,
    },
    Forget {
        route: String,
    },
    RouteLayout {
        route: String,
        #[arg(required = true, num_args = 1..)]
        layout: Vec<String>,
    },
    Sources,
    Layout {
        source: String,
        #[arg(required = true, num_args = 1..)]
        layout: Vec<String>,
    },
    Wire {
        from: String,
        output: String,
        #[arg(value_name = "OUT=IN[@DB]")]
        channels: Vec<String>,
        #[arg(long, value_name = "DB", allow_hyphen_values = true)]
        gain: Option<f32>,
        #[arg(long)]
        mute: bool,
    },
    WireGain {
        from: String,
        output: String,
        #[arg(allow_hyphen_values = true)]
        gain: String,
    },
    Unwire {
        from: String,
        output: String,
        #[arg(value_name = "OUT=IN")]
        channels: Vec<String>,
    },
    Patch {
        #[arg(long, value_name = "SECONDS", num_args = 0..=1, default_missing_value = "1")]
        watch: Option<f64>,
    },
    ForgetSource {
        source: String,
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
    routes: Vec<Sender>,
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
    running: bool,
    error: Option<String>,
    holdover: bool,
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
    /// Plays nowhere: the default output is another node's sink.
    #[serde(default)]
    held: bool,
    /// Plays on wires, not on an output of its own.
    #[serde(default)]
    wired: bool,
    /// Plays nowhere until its output is back or it is played elsewhere.
    parked: Option<Parked>,
    /// What the stream's channels carry.
    layout: Option<Layout>,
    /// The channels of the output it plays on.
    output_channels: Option<Layout>,
    /// How the audio system mixes the stream onto the output: `downmix`,
    /// `upmix` or `remix`; absent where the channels match.
    mix: Option<String>,
}

/// Why a stream plays nowhere.
#[derive(Clone, Deserialize)]
struct Parked {
    /// `lost` or `unwired`.
    why: String,
    /// The output that went, as it was called.
    description: Option<String>,
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

#[derive(Subcommand)]
enum OutputAction {
    Create {
        name: String,
        device: String,
        #[arg(required = true, num_args = 1..)]
        channels: Vec<usize>,
        #[arg(long)]
        layout: Option<String>,
    },
    Remove {
        output: String,
    },
}

/// An input of a node.
#[derive(Deserialize)]
struct InputEntry {
    name: String,
    description: String,
    /// `receiver` or `device`.
    kind: String,
    channels: Layout,
}

/// Where a created output plays.
#[derive(Clone, Deserialize)]
struct CreatedOn {
    device: String,
    channels: Vec<usize>,
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
    channels: Layout,
    created: Option<CreatedOn>,
}

#[derive(Clone, Deserialize)]
struct Sender {
    /// Route ID.
    id: String,
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
    /// The sink is gone for now, while the peer can't play the stream.
    #[serde(default)]
    hidden: bool,
}

#[derive(Clone, Deserialize)]
struct PeerLink {
    state: String,
    /// `muted` or `ducked` by the peer's policy.
    policy: Option<String>,
    /// `lost` or `unwired`: the peer plays the stream nowhere.
    parked: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    layout: Option<Layout>,
}

#[derive(Deserialize)]
struct Created {
    id: String,
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
        d.routes = self
            .routes
            .iter()
            .map(|t| {
                // A new route counts from now.
                let p = prev.routes.iter().find(|p| p.id == t.id).unwrap_or(t);
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
    let cli = Cli::from_arg_matches(&i18n::command().get_matches()).unwrap_or_else(|e| e.exit());
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
            anyhow::ensure!(secs >= 0.1, t!("watch-too-short"));
            watch(base, Duration::from_secs_f64(secs), cli.json)?;
        }
        Command::Connect {
            peer,
            name,
            send_only,
            force,
            priority,
            layout,
        } => {
            let layout = layout
                .map(|l| Layout::parse(&l).map_err(|e| anyhow::anyhow!("{e}")))
                .transpose()?;
            let url = format!("{base}/x-orpheus/v1/routes");
            let raw = post(
                &url,
                NewSender {
                    peer: peer.clone(),
                    name,
                    send_only,
                    force,
                    priority,
                    layout,
                },
            )
            .with_context(|| t!("connecting-via", peer = &peer, url = &url))?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let c: Created =
                    serde_json::from_str(&raw).with_context(|| t!("reading-reply", url = &url))?;
                let receiver = if c.receiver.is_some() { "yes" } else { "no" };
                let created = t!(
                    "route-created",
                    id = &c.id,
                    name = c.name.as_deref().unwrap_or(&peer),
                    to = c.destination.as_deref().unwrap_or(&peer),
                    receiver = receiver,
                );
                println!("{created}");
            }
        }
        Command::Nodes => {
            let url = format!("{base}/x-orpheus/v1/nodes");
            let raw = get(&url)?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let nodes: Vec<Node> = serde_json::from_str(&raw)
                    .with_context(|| t!("reading-from", what = "nodes", url = &url))?;
                print!("{}", render_nodes(&nodes, Paint::detect()));
            }
        }
        Command::Outputs => {
            let raw = get(&format!("{base}/x-orpheus/v1/outputs"))?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let outputs: Vec<Output> =
                    serde_json::from_str(&raw).with_context(|| t!("reading", what = "outputs"))?;
                print!("{}", render_outputs(&outputs, Paint::detect()));
            }
        }
        Command::Inputs => {
            let raw = get(&format!("{base}/x-orpheus/v1/inputs"))?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let inputs: Vec<InputEntry> =
                    serde_json::from_str(&raw).with_context(|| t!("reading", what = "inputs"))?;
                print!("{}", render_inputs(&inputs, Paint::detect()));
            }
        }
        Command::Output {
            action:
                OutputAction::Create {
                    name,
                    device,
                    channels,
                    layout,
                },
        } => {
            let outputs: Vec<Output> =
                serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/outputs"))?)
                    .with_context(|| t!("reading", what = "outputs"))?;
            let device = find_output(&outputs, &device)?;
            let mut body = serde_json::json!({
                "name": name, "device": device.name, "channels": channels,
            });
            if let Some(layout) = layout {
                let layout = Layout::parse(&layout).map_err(|e| anyhow::anyhow!("{e}"))?;
                body["layout"] = serde_json::to_value(layout)?;
            }
            let url = format!("{base}/x-orpheus/v1/outputs");
            let raw = reply(agent().post(&url).send_json(body)?)
                .with_context(|| t!("creating-via", name = &name, url = &url))?;
            let created: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            if let Some(name) = created["name"].as_str() {
                println!("{}", t!("output-created", name = name));
            }
        }
        Command::Output {
            action: OutputAction::Remove { output },
        } => {
            let outputs: Vec<Output> =
                serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/outputs"))?)
                    .with_context(|| t!("reading", what = "outputs"))?;
            let o = find_output(&outputs, &output)?;
            let url = format!("{base}/x-orpheus/v1/outputs/{}", encode(&o.name));
            reply(agent().delete(&url).call()?)
                .with_context(|| t!("removing-via", what = &output, url = &url))?;
        }
        Command::SetType { output, kind } => {
            let outputs: Vec<Output> =
                serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/outputs"))?)
                    .with_context(|| t!("reading", what = "outputs"))?;
            let o = find_output(&outputs, &output)?;
            let url = format!("{base}/x-orpheus/v1/outputs/{}/type", encode(&o.name));
            if kind == "auto" {
                let r = agent().delete(&url).call()?;
                // Nothing to reset.
                if r.status() != 404 {
                    reply(r).with_context(|| t!("resetting-via", url = &url))?;
                }
            } else {
                let r = agent()
                    .put(&url)
                    .send_json(serde_json::json!({ "type": kind }))?;
                reply(r).with_context(|| t!("setting-via", what = &kind, url = &url))?;
            }
        }
        Command::Play { receiver, output } => {
            let url = format!("{base}/x-orpheus/v1/receivers/{receiver}/output");
            if output == "default" {
                let r = agent().delete(&url).call()?;
                reply(r).with_context(|| t!("moving-via", url = &url))?;
            } else {
                let outputs: Vec<Output> =
                    serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/outputs"))?)
                        .with_context(|| t!("reading", what = "outputs"))?;
                let o = find_output(&outputs, &output)?;
                let r = agent()
                    .put(&url)
                    .send_json(serde_json::json!({ "output": o.name }))?;
                reply(r).with_context(|| t!("moving-via", url = &url))?;
            }
        }
        Command::Default { output } => {
            let outputs: Vec<Output> =
                serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/outputs"))?)
                    .with_context(|| t!("reading", what = "outputs"))?;
            let o = find_output(&outputs, &output)?;
            let url = format!("{base}/x-orpheus/v1/outputs/{}/default", encode(&o.name));
            let r = agent().put(&url).send_empty()?;
            reply(r).with_context(|| t!("setting-default-via", url = &url))?;
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
                    .with_context(|| t!("expected-setting", arg = s))?;
                let (section, key) = path
                    .split_once('.')
                    .with_context(|| t!("expected-key", arg = path))?;
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
        Command::Routes => {
            let url = format!("{base}/x-orpheus/v1/routes");
            let raw = get(&url)?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let routes: Vec<RouteEntry> = serde_json::from_str(&raw)
                    .with_context(|| t!("reading-from", what = "routes", url = &url))?;
                print!("{}", render_routes(&routes, Paint::detect(), unix_now()));
            }
        }
        Command::Disconnect { route } => {
            let id = find_route(base, &route)?;
            let url = format!("{base}/x-orpheus/v1/routes/{id}");
            reply(agent().delete(&url).call()?)
                .with_context(|| t!("disconnecting-via", route = &route, url = &url))?;
        }
        Command::Forget { route } => {
            let id = find_route(base, &route)?;
            let url = format!("{base}/x-orpheus/v1/routes/{id}?forget=true");
            reply(agent().delete(&url).call()?)
                .with_context(|| t!("forgetting-via", what = &route, url = &url))?;
        }
        Command::RouteLayout { route, layout } => {
            let layout = Layout::parse(&layout.join(" ")).map_err(|e| anyhow::anyhow!("{e}"))?;
            let id = find_route(base, &route)?;
            let url = format!("{base}/x-orpheus/v1/routes/{id}/layout");
            reply(
                agent()
                    .put(&url)
                    .send_json(serde_json::json!({ "layout": layout }))?,
            )
            .with_context(|| t!("setting-layout-via", what = &route, url = &url))?;
        }
        Command::Sources => {
            let url = format!("{base}/x-orpheus/v1/sources");
            let raw = get(&url)?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let sources: Vec<SourceEntry> = serde_json::from_str(&raw)
                    .with_context(|| t!("reading-from", what = "sources", url = &url))?;
                // Only to show their names.
                let outputs: Vec<Output> = get(&format!("{base}/x-orpheus/v1/outputs"))
                    .ok()
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or_default();
                print!(
                    "{}",
                    render_sources(&sources, &outputs, Paint::detect(), unix_now())
                );
            }
        }
        Command::Layout { source, layout } => {
            let id = find_source(base, &source)?;
            let url = format!("{base}/x-orpheus/v1/sources/{id}/layout");
            let text = layout.join(" ");
            let r = if text == "auto" {
                agent().delete(&url).call()?
            } else {
                let layout = Layout::parse(&text).map_err(|e| anyhow::anyhow!("{e}"))?;
                agent()
                    .put(&url)
                    .send_json(serde_json::json!({ "layout": layout }))?
            };
            reply(r).with_context(|| t!("setting-layout-via", what = &source, url = &url))?;
        }
        Command::Wire {
            from,
            output,
            channels,
            gain,
            mute,
        } => {
            let mut body = wire_ends(base, &from, &output)?;
            if let Some(db) = gain {
                body["gain_db"] = serde_json::json!(db);
            }
            if mute {
                body["mute"] = serde_json::json!(true);
            }
            if !channels.is_empty() {
                let pairs = channels
                    .iter()
                    .map(|c| parse_pair(c))
                    .collect::<Result<Vec<_>>>()?;
                body["channels"] = serde_json::Value::Array(pairs);
            }
            let url = format!("{base}/x-orpheus/v1/wires");
            let raw = reply(agent().put(&url).send_json(&body)?)
                .with_context(|| t!("wiring-via", from = &from, output = &output, url = &url))?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let wire: WireEntry =
                    serde_json::from_str(&raw).with_context(|| t!("reading", what = "wire"))?;
                print!("{}", render_wire_set(&wire, Paint::detect()));
            }
        }
        Command::WireGain { from, output, gain } => {
            let body = match gain.as_str() {
                "mute" => serde_json::json!({ "mute": true }),
                "unmute" => serde_json::json!({ "mute": false }),
                db => {
                    let db: f32 = db
                        .trim_end_matches("dB")
                        .trim()
                        .parse()
                        .with_context(|| t!("not-a-gain", gain = &gain))?;
                    serde_json::json!({ "gain_db": db })
                }
            };
            let ends = wire_ends(base, &from, &output)?;
            let url = format!("{base}/x-orpheus/v1/wires/gain");
            let mut req = agent()
                .put(&url)
                .query("output", ends["output"].as_str().unwrap_or_default());
            for key in ["source", "input"] {
                if let Some(v) = ends[key].as_str() {
                    req = req.query(key, v);
                }
            }
            let raw = reply(req.send_json(&body)?).with_context(|| {
                t!(
                    "setting-gain-via",
                    from = &from,
                    output = &output,
                    url = &url
                )
            })?;
            if cli.json {
                println!("{}", raw.trim_end());
            } else {
                let wire: WireEntry =
                    serde_json::from_str(&raw).with_context(|| t!("reading", what = "wire"))?;
                print!("{}", render_wire_set(&wire, Paint::detect()));
            }
        }
        Command::Unwire {
            from,
            output,
            channels,
        } => {
            let ends = wire_ends(base, &from, &output)?;
            let url = format!("{base}/x-orpheus/v1/wires");
            let mut query = vec![("output", ends["output"].as_str().unwrap_or_default())];
            for key in ["source", "input"] {
                if let Some(v) = ends[key].as_str() {
                    query.push((key, v));
                }
            }
            if channels.is_empty() {
                let mut req = agent().delete(&url);
                for (k, v) in query {
                    req = req.query(k, v);
                }
                reply(req.call()?)
                    .with_context(|| t!("unwiring", from = &from, output = &output))?;
                return Ok(());
            }
            let wires: Vec<WireEntry> =
                serde_json::from_str(&get(&url)?).with_context(|| t!("reading", what = "wires"))?;
            let wire = wires
                .iter()
                .find(|w| {
                    w.output == ends["output"]
                        && (w.source.as_deref() == ends["source"].as_str())
                        && (w.input.as_deref() == ends["input"].as_str())
                })
                .with_context(|| t!("no-wire", from = &from, output = &output))?;
            let Some(have) = &wire.channels else {
                anyhow::bail!(t!("wire-plays-all", from = &from, output = &output));
            };
            let mut keep = have.clone();
            for c in &channels {
                let pair = parse_pair(c)?;
                let same = |p: &serde_json::Value| {
                    ["to", "from"].iter().all(|k| {
                        p[k].to_string().trim_matches('"').to_lowercase()
                            == pair[k].to_string().trim_matches('"').to_lowercase()
                    })
                };
                let before = keep.len();
                keep.retain(|p| !same(p));
                if keep.len() == before {
                    anyhow::bail!(t!("wire-lacks", pair = c));
                }
            }
            if keep.is_empty() {
                let mut req = agent().delete(&url);
                for (k, v) in query {
                    req = req.query(k, v);
                }
                reply(req.call()?)
                    .with_context(|| t!("unwiring", from = &from, output = &output))?;
            } else {
                let mut body = ends.clone();
                body["channels"] = serde_json::Value::Array(keep);
                reply(agent().put(&url).send_json(&body)?)
                    .with_context(|| t!("unwiring-channels-via", url = &url))?;
            }
        }
        Command::Patch { watch: None } => print!("{}", patch(base, cli.json, Paint::detect())?),
        Command::Patch { watch: Some(secs) } => {
            anyhow::ensure!(secs >= 0.1, t!("watch-too-short"));
            watch_patch(base, Duration::from_secs_f64(secs), cli.json)?;
        }
        Command::ForgetSource { source } => {
            let id = find_source(base, &source)?;
            let url = format!("{base}/x-orpheus/v1/sources/{id}");
            reply(agent().delete(&url).call()?)
                .with_context(|| t!("forgetting-via", what = &source, url = &url))?;
        }
    }
    Ok(())
}

/// A route, as `/x-orpheus/v1/routes` lists it.
#[derive(Deserialize)]
struct RouteEntry {
    id: String,
    name: String,
    priority: String,
    peer: String,
    active: bool,
    /// How a running route to a peer's receiver stands.
    state: Option<String>,
    #[serde(default)]
    layout: Option<Layout>,
    last_used: Option<u64>,
}

/// The ID of the route `query` names: by ID, the start of an ID, or name.
fn find_route(base: &str, query: &str) -> Result<String> {
    let routes: Vec<RouteEntry> =
        serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/routes"))?)
            .with_context(|| t!("reading", what = "routes"))?;
    let ids: Vec<(&str, &str)> = routes
        .iter()
        .map(|r| (r.name.as_str(), r.id.as_str()))
        .collect();
    pick(&ids, query, "routes")
}

/// The ID of the source `query` names: by ID, the start of an ID, or name.
fn find_source(base: &str, query: &str) -> Result<String> {
    let sources: Vec<SourceEntry> =
        serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/sources"))?)
            .with_context(|| t!("reading", what = "sources"))?;
    let ids: Vec<(&str, &str)> = sources
        .iter()
        .map(|s| (s.name.as_str(), s.id.as_str()))
        .collect();
    pick(&ids, query, "sources")
}

/// The ID of the one of `items` (name, ID) that `query` names: by name,
/// else by ID or the start of one, read as the daemon reads IDs: any
/// case, `O` as 0, `I` and `L` as 1, hyphens ignored. `list` is the
/// command that lists them, `routes` or `sources`.
fn pick(items: &[(&str, &str)], query: &str, list: &str) -> Result<String> {
    let q = query.to_lowercase();
    let by_name: Vec<&(&str, &str)> = items.iter().filter(|i| i.0.to_lowercase() == q).collect();
    let found = match by_name[..] {
        [] => {
            let id: String = query
                .chars()
                .filter(|&c| c != '-')
                .map(|c| match c.to_ascii_uppercase() {
                    'O' => '0',
                    'I' | 'L' => '1',
                    c => c,
                })
                .collect();
            items.iter().filter(|i| i.1.starts_with(&id)).collect()
        }
        _ => by_name,
    };
    match found[..] {
        [i] => Ok(i.1.to_owned()),
        [] => anyhow::bail!(t!("pick-none", list = list, query = query)),
        _ => {
            let ids = found.iter().map(|i| i.1).collect::<Vec<_>>().join(", ");
            anyhow::bail!(t!("pick-many", query = query, list = list, ids = ids))
        }
    }
}

/// The ends of a wire as the API takes them: `{"source": ID}` or
/// `{"input": NAME}`, and `{"output": NAME}`.
fn wire_ends(base: &str, from: &str, output: &str) -> Result<serde_json::Value> {
    let sources: Vec<SourceEntry> =
        serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/sources"))?)
            .with_context(|| t!("reading", what = "sources"))?;
    let inputs: Vec<InputEntry> =
        serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/inputs"))?)
            .with_context(|| t!("reading", what = "inputs"))?;
    let outputs: Vec<Output> = serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/outputs"))?)
        .with_context(|| t!("reading", what = "outputs"))?;
    let output = &find_output(&outputs, output)?.name;
    // Receivers' streams are wired by their sources.
    let devices: Vec<&InputEntry> = inputs.iter().filter(|i| i.kind == "device").collect();
    let q = from.to_lowercase();
    let source_named: Vec<&SourceEntry> = sources
        .iter()
        .filter(|s| s.name.to_lowercase() == q)
        .collect();
    let input_named: Vec<&&InputEntry> = devices
        .iter()
        .filter(|i| i.name == from || i.description.to_lowercase() == q)
        .collect();
    let found = match (&source_named[..], &input_named[..]) {
        ([s], []) => serde_json::json!({ "source": s.id }),
        ([], [i]) => serde_json::json!({ "input": i.name }),
        ([], []) => {
            let ids: Vec<(&str, &str)> = sources
                .iter()
                .map(|s| (s.name.as_str(), s.id.as_str()))
                .collect();
            let by_part: Vec<&&InputEntry> = devices
                .iter()
                .filter(|i| {
                    i.name.to_lowercase().contains(&q) || i.description.to_lowercase().contains(&q)
                })
                .collect();
            match (pick(&ids, from, "sources"), &by_part[..]) {
                (Ok(id), []) => serde_json::json!({ "source": id }),
                (Err(_), [i]) => serde_json::json!({ "input": i.name }),
                (Err(_), []) => anyhow::bail!(t!("no-source-or-input", from = from)),
                _ => anyhow::bail!(t!("from-ambiguous", from = from)),
            }
        }
        _ => anyhow::bail!(t!("from-ambiguous-id", from = from)),
    };
    let mut body = found;
    body["output"] = serde_json::Value::String(output.clone());
    Ok(body)
}

/// `OUT=IN[@DB]` → `{"to": OUT, "from": IN, "gain_db": DB}`, numbers as
/// numbers.
fn parse_pair(text: &str) -> Result<serde_json::Value> {
    let (pair, gain) = match text.rsplit_once('@') {
        Some((pair, db)) => {
            let db: f32 = db
                .trim_end_matches("dB")
                .trim()
                .parse()
                .with_context(|| t!("bad-gain-in-pair", pair = text, db = db))?;
            (pair, Some(db))
        }
        None => (text, None),
    };
    let (to, from) = pair
        .split_once('=')
        .filter(|(t, f)| !t.trim().is_empty() && !f.trim().is_empty())
        .with_context(|| t!("bad-pair", pair = text))?;
    let channel = |c: &str| {
        let c = c.trim();
        match c.parse::<usize>() {
            Ok(n) => serde_json::json!(n),
            Err(_) => serde_json::json!(c),
        }
    };
    let mut p = serde_json::json!({ "to": channel(to), "from": channel(from) });
    if let Some(db) = gain {
        p["gain_db"] = serde_json::json!(db);
    }
    Ok(p)
}

/// A gain for people: `-3 dB`, `+2.5 dB`.
fn db_text(db: f32) -> String {
    if db > 0.0 {
        format!("+{} dB", num(db.to_string()))
    } else {
        format!("{} dB", num(db.to_string()))
    }
}

/// A wire, as `/x-orpheus/v1/wires` lists it.
#[derive(Deserialize)]
struct WireEntry {
    source: Option<String>,
    input: Option<String>,
    from_name: String,
    output: String,
    output_name: String,
    /// `{"to": …, "from": …}` as set; `None` for all channels.
    channels: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    gain_db: f32,
    #[serde(default)]
    mute: bool,
    links: Vec<WireLink>,
    /// `downmix` or `upmix`; absent where each channel plays on its own.
    mix: Option<String>,
    /// Input channels left out, from 1.
    #[serde(default)]
    dropped: Vec<usize>,
    from_channels: Option<Layout>,
    output_channels: Option<Layout>,
}

#[derive(Deserialize)]
struct WireLink {
    from: usize,
    to: usize,
    linked: bool,
    /// The wire's and the pair's together.
    #[serde(default)]
    gain_db: f32,
    /// Plays the channel on another one as part of a mix.
    #[serde(default)]
    folded: bool,
}

/// Marks a channel played on another one as part of a mix.
const FOLDED: &str = "⤵";

/// A mix in words, `downmix 51 → ST`, with what a wire leaves out.
fn mix_text(mix: &str, from: Option<&Layout>, to: Option<&Layout>, dropped: &[usize]) -> String {
    let mix = t!("mix", mix = mix);
    let text = match (from, to) {
        (Some(f), Some(to)) => t!("mix-layouts", mix = mix, from = short(f), to = short(to)),
        _ => mix,
    };
    if dropped.is_empty() {
        return text;
    }
    let names: Vec<String> = dropped.iter().map(|&c| channel_name(from, c)).collect();
    t!("mix-left-out", mix = text, channels = names.join(" "))
}

impl WireEntry {
    /// Its own gain, `-6 dB, muted`; `None` at 0 dB.
    fn gain_text(&self) -> Option<String> {
        match (self.gain_db, self.mute) {
            (0.0, false) => None,
            (0.0, true) => Some(t!("muted")),
            (db, false) => Some(db_text(db)),
            (db, true) => Some(t!("gain-muted", gain = db_text(db))),
        }
    }

    fn mix_text(&self) -> Option<String> {
        let mix = self.mix.as_deref()?;
        Some(mix_text(
            mix,
            self.from_channels.as_ref(),
            self.output_channels.as_ref(),
            &self.dropped,
        ))
    }
}

/// Channel `n` (from 1) of `layout`: its symbol, with its label if it has
/// one, `Mic 3 (U03)`; its number if the layout isn't known.
fn channel_name(layout: Option<&Layout>, n: usize) -> String {
    match layout.and_then(|l| l.channels().get(n - 1)) {
        Some(c) => match &c.label {
            Some(label) => format!("{label} ({})", c.symbol),
            None => c.symbol.to_string(),
        },
        None => n.to_string(),
    }
}

/// Why a wire links nothing.
fn waiting(w: &WireEntry) -> String {
    if w.from_channels.is_none() {
        match w.source {
            Some(_) => t!("plays-nowhere", name = &w.from_name),
            None => t!("is-missing", name = &w.from_name),
        }
    } else if w.output_channels.is_none() {
        t!("is-missing", name = &w.output_name)
    } else {
        t!("no-channels-match")
    }
}

/// What `wire` set.
fn render_wire_set(w: &WireEntry, p: Paint) -> String {
    let mut o = String::new();
    let _ = write!(o, "{} → {}", w.from_name, w.output_name);
    if let Some(gain) = w.gain_text() {
        let _ = write!(o, " ({gain})");
    }
    if w.links.is_empty() {
        let _ = writeln!(
            o,
            ": {}",
            p.level(Level::Warn, &t!("waiting-because", why = waiting(w)))
        );
        return o;
    }
    let pairs: Vec<String> = w
        .links
        .iter()
        .map(|l| {
            // The pair's own gain, as `wire` takes it.
            let own = l.gain_db - w.gain_db;
            format!(
                "{}={}{}{}",
                channel_name(w.output_channels.as_ref(), l.to),
                channel_name(w.from_channels.as_ref(), l.from),
                if l.folded { FOLDED } else { "" },
                if own == 0.0 {
                    String::new()
                } else {
                    format!("@{}", num(format!("{own:+}")))
                }
            )
        })
        .collect();
    let _ = write!(o, ": {}", pairs.join(" "));
    match w.mix_text() {
        Some(mix) => writeln!(o, " ({mix})"),
        None => writeln!(o),
    }
    .ok();
    o
}

/// The wires as a patch bay: per output channel, everything playing on
/// it; then the wires that link nothing yet.
/// The patch as text, or with `json` the wires and levels as the node
/// gives them.
fn patch(base: &str, json: bool, p: Paint) -> Result<String> {
    let wires = get(&format!("{base}/x-orpheus/v1/wires"))?;
    let levels = get(&format!("{base}/x-orpheus/v1/levels"))?;
    if json {
        let both = serde_json::json!({
            "wires": serde_json::from_str::<serde_json::Value>(&wires)?,
            "levels": serde_json::from_str::<serde_json::Value>(&levels)?,
        });
        return Ok(format!("{both}\n"));
    }
    let wires: Vec<WireEntry> =
        serde_json::from_str(&wires).with_context(|| t!("reading", what = "wires"))?;
    let levels: Levels =
        serde_json::from_str(&levels).with_context(|| t!("reading", what = "levels"))?;
    Ok(render_patch(&wires, Some(&levels), p))
}

/// Shows the patch every `interval`, until stopped.
fn watch_patch(base: &str, interval: Duration, json: bool) -> Result<()> {
    let paint = Paint::detect();
    let mut screen = Screen {
        out: std::io::stdout().lock(),
        tty: std::io::stdout().is_terminal(),
        json,
        first: true,
    };
    loop {
        let frame = patch(base, json, paint).unwrap_or_else(|e| render_error(&e, base, paint));
        screen.show(&frame).map_err(|Unwritable(e)| e)?;
        std::thread::sleep(interval);
    }
}

/// Levels, as `/x-orpheus/v1/levels` gives them.
#[derive(Deserialize, Default)]
struct Levels {
    outputs: Vec<OutputLevels>,
    wires: Vec<WireLevels>,
}

#[derive(Deserialize)]
struct OutputLevels {
    output: String,
    channels: Vec<LevelEntry>,
    clipped_secs_ago: Option<f64>,
}

#[derive(Deserialize)]
struct WireLevels {
    source: Option<String>,
    input: Option<String>,
    output: String,
    links: Vec<LinkLevel>,
}

#[derive(Deserialize)]
struct LinkLevel {
    from: usize,
    to: usize,
    #[serde(flatten)]
    level: LevelEntry,
}

/// Absent while silent.
#[derive(Deserialize)]
struct LevelEntry {
    peak_db: Option<f32>,
}

impl Levels {
    fn output(&self, name: &str) -> Option<&OutputLevels> {
        self.outputs.iter().find(|o| o.output == name)
    }

    /// The level of what `w` brings from channel `from` to channel `to`.
    fn link(&self, w: &WireEntry, from: usize, to: usize) -> Option<&LevelEntry> {
        self.wires
            .iter()
            .find(|l| l.source == w.source && l.input == w.input && l.output == w.output)?
            .links
            .iter()
            .find(|l| l.from == from && l.to == to)
            .map(|l| &l.level)
    }
}

/// How wide the level column of `patch` is.
const LEVEL_WIDTH: usize = 10;

/// A peak in dBFS, `-12.5 dBFS`, marked once it reaches full scale;
/// right-aligned to `width`.
fn peak_text(level: Option<&LevelEntry>, width: usize, p: Paint) -> String {
    let peak = level.and_then(|l| l.peak_db);
    let text = peak.map_or_else(
        || t!("silent"),
        |db| format!("{} dBFS", num(format!("{db:.1}"))),
    );
    let text = format!("{text:>width$}");
    match peak {
        Some(db) if db >= 0.0 => p.level(Level::Bad, &text),
        Some(_) => text,
        None => p.dim(&text),
    }
}

fn render_patch(wires: &[WireEntry], levels: Option<&Levels>, p: Paint) -> String {
    let mut o = String::new();
    if wires.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, &t!("no-wires")));
        return o;
    }
    let mut outputs: Vec<(&str, &str)> = Vec::new();
    for w in wires.iter().filter(|w| !w.links.is_empty()) {
        if !outputs.iter().any(|(name, _)| *name == w.output) {
            outputs.push((&w.output, &w.output_name));
        }
    }
    for (i, (name, description)) in outputs.iter().enumerate() {
        if i > 0 {
            o.push('\n');
        }
        let output = levels.and_then(|l| l.output(name));
        let clipped = output
            .and_then(|o| o.clipped_secs_ago)
            .map(|secs| {
                let text = t!("clipped-ago", secs = format!("{secs:.0}"));
                format!("  {}", p.level(Level::Bad, &text))
            })
            .unwrap_or_default();
        let _ = writeln!(
            o,
            "{} {}{clipped}",
            p.bold(description),
            p.dim(&format!("({name})"))
        );
        let on: Vec<&WireEntry> = wires.iter().filter(|w| w.output == *name).collect();
        let layout = on.iter().find_map(|w| w.output_channels.as_ref());
        let mut channels: Vec<usize> = on
            .iter()
            .flat_map(|w| w.links.iter().map(|l| l.to))
            .collect();
        channels.sort_unstable();
        channels.dedup();
        let names: Vec<String> = channels.iter().map(|&c| channel_name(layout, c)).collect();
        let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
        for (c, label) in channels.iter().zip(&names) {
            let feeds: Vec<String> = on
                .iter()
                .flat_map(|w| {
                    w.links.iter().filter(|l| l.to == *c).map(move |l| {
                        let gain = if w.mute {
                            format!(" {}", t!("muted"))
                        } else if l.gain_db != 0.0 {
                            format!(" {}", db_text(l.gain_db))
                        } else {
                            String::new()
                        };
                        let level = levels
                            .map(|v| format!(" [{}]", peak_text(v.link(w, l.from, *c), 0, p)))
                            .unwrap_or_default();
                        let text = format!(
                            "{}: {}{}{gain}{level}",
                            w.from_name,
                            channel_name(w.from_channels.as_ref(), l.from),
                            if l.folded { FOLDED } else { "" }
                        );
                        if l.linked {
                            text
                        } else {
                            p.level(Level::Warn, &t!("linking", wire = text))
                        }
                    })
                })
                .collect();
            let level = match levels {
                // The output measures nothing without monitor ports.
                Some(_) => match output {
                    Some(out) => {
                        format!("  {}", peak_text(out.channels.get(c - 1), LEVEL_WIDTH, p))
                    }
                    None => format!("  {:>LEVEL_WIDTH$}", "—"),
                },
                None => String::new(),
            };
            let _ = writeln!(o, "  {label:<width$}{level}  ← {}", feeds.join(", "));
        }
        for w in on.iter().filter(|w| !w.links.is_empty()) {
            if let Some(mix) = w.mix_text() {
                let _ = writeln!(o, "  {FOLDED} {}: {mix}", w.from_name);
            }
        }
    }
    let idle: Vec<&WireEntry> = wires.iter().filter(|w| w.links.is_empty()).collect();
    if !idle.is_empty() {
        if !outputs.is_empty() {
            o.push('\n');
        }
        let _ = writeln!(o, "{}", p.bold(&t!("heading-waiting")));
        for w in idle {
            let channels = match &w.channels {
                None => t!("all-channels"),
                Some(pairs) => pairs
                    .iter()
                    .map(|p| {
                        let side = |k: &str| p[k].to_string().trim_matches('"').to_owned();
                        format!("{}={}", side("to"), side("from"))
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            let _ = writeln!(
                o,
                "  {} → {} ({channels}): {}",
                w.from_name,
                w.output_name,
                p.level(Level::Warn, &waiting(w))
            );
        }
    }
    o
}

/// A source, as `/x-orpheus/v1/sources` lists it.
#[derive(Deserialize)]
struct SourceEntry {
    id: String,
    name: String,
    output: Option<String>,
    volume: Option<Volume>,
    /// Set by the user; absent for what the stream says.
    layout: Option<Layout>,
    receiver: Option<u32>,
    last_used: u64,
}

#[derive(Deserialize)]
struct Volume {
    channels: Vec<f32>,
    mute: bool,
}

impl Volume {
    /// As mixers show it: the loudest channel, on a cubic scale.
    fn percent(&self) -> String {
        if self.mute {
            return t!("muted");
        }
        let max = self.channels.iter().copied().fold(0.0f32, f32::max);
        format!("{:.0}%", max.cbrt() * 100.0)
    }
}

/// The sources a node's receivers remember; `outputs` names the outputs.
fn render_sources(sources: &[SourceEntry], outputs: &[Output], p: Paint, now: u64) -> String {
    let mut o = String::new();
    if sources.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, &t!("no-sources")));
        return o;
    }
    let rows: Vec<[String; 6]> = sources
        .iter()
        .map(|s| {
            let output = match &s.output {
                Some(name) => outputs
                    .iter()
                    .find(|o| o.name == *name)
                    .map_or(name.clone(), |o| o.description.clone()),
                None => t!("default"),
            };
            let state = match s.receiver {
                Some(r) => p.mark(Level::Good, &t!("playing-on-receiver", receiver = r)),
                None => {
                    let ago = t!("ago", days = now.saturating_sub(s.last_used) / 86_400);
                    p.mark(Level::Off, &t!("last-played", ago = ago))
                }
            };
            [
                s.name.clone(),
                s.id.clone(),
                output,
                s.volume.as_ref().map_or("100%".into(), Volume::percent),
                s.layout
                    .as_ref()
                    .map_or_else(|| t!("as-sent"), ToString::to_string),
                state,
            ]
        })
        .collect();
    let header = [
        t!("col-name"),
        t!("col-id"),
        t!("col-output"),
        t!("col-volume"),
        t!("col-layout"),
        t!("col-state"),
    ];
    table(
        &mut o,
        p,
        header,
        [false, false, false, true, false, false],
        &rows,
    );
    o
}

/// Seconds since the Unix epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The routes a node runs and remembers.
fn render_routes(routes: &[RouteEntry], p: Paint, now: u64) -> String {
    let mut o = String::new();
    if routes.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, &t!("no-routes")));
        return o;
    }
    let rows: Vec<[String; 6]> = routes
        .iter()
        .map(|r| {
            let state = match (r.active, r.last_used) {
                (true, _) => route_state(p, r.state.as_deref()),
                (false, Some(used)) => {
                    let ago = t!("ago", days = now.saturating_sub(used) / 86_400);
                    p.mark(Level::Off, &t!("remembered-last-used", ago = ago))
                }
                (false, None) => p.mark(Level::Off, &t!("remembered")),
            };
            [
                r.name.clone(),
                r.id.clone(),
                r.peer.clone(),
                r.layout.as_ref().map_or_else(|| "-".into(), short),
                r.priority.clone(),
                state,
            ]
        })
        .collect();
    let header = [
        t!("col-name"),
        t!("col-id"),
        t!("col-peer"),
        t!("col-channels"),
        t!("col-priority"),
        t!("col-state"),
    ];
    table(&mut o, p, header, [false; 6], &rows);
    o
}

/// How a running route stands, marked.
fn route_state(p: Paint, state: Option<&str>) -> String {
    match state {
        None | Some("connected") => p.mark(Level::Good, &t!("route-connected")),
        Some("idle") => p.mark(Level::Off, &t!("route-idle")),
        Some("busy") => p.mark(Level::Warn, &t!("route-busy")),
        Some("incompatible") => p.mark(Level::Warn, &t!("route-incompatible")),
        Some("offline") => p.mark(Level::Off, &t!("route-offline")),
        Some("unreachable") => p.mark(Level::Warn, &t!("route-unreachable")),
        Some("disconnected") => p.mark(Level::Warn, &t!("route-disconnected")),
        Some(s) => p.mark(Level::Off, s),
    }
}

/// The device ID of the Orpheus node `query` names: by ID, alias or name.
fn find_node(base: &str, query: &str) -> Result<String> {
    let nodes: Vec<Node> = serde_json::from_str(&get(&format!("{base}/x-orpheus/v1/nodes"))?)
        .with_context(|| t!("reading", what = "nodes"))?;
    let is = |s: &Option<String>| s.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(query));
    let found = nodes
        .iter()
        .find(|n| is(&n.id))
        .or_else(|| nodes.iter().find(|n| is(&n.alias)))
        .or_else(|| nodes.iter().find(|n| n.name.eq_ignore_ascii_case(query)))
        .with_context(|| t!("no-node", query = query))?;
    found
        .id
        .clone()
        .with_context(|| t!("not-orpheus", query = query))
}

/// Changes another node's alias or pin.
fn change_node(base: &str, id: &str, body: serde_json::Value) -> Result<()> {
    let url = format!("{base}/x-orpheus/v1/nodes/{id}");
    let r = agent()
        .patch(&url)
        .send_json(body)
        .with_context(|| t!("changing-node-via", id = id, url = &url))?;
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
        .with_context(|| t!("changing-settings-via", url = &url))?;
    reply(r)
}

fn print_settings(raw: &str, json: bool) -> Result<()> {
    if json {
        println!("{}", raw.trim_end());
    } else {
        let v: serde_json::Value =
            serde_json::from_str(raw).with_context(|| t!("reading", what = "settings"))?;
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
    // Nodes answer errors in JSON with an "error" field; anything else in
    // between, such as a proxy, may answer in plain text.
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
        let refused = t!("refused", message = &self.message, status = self.status);
        f.write_str(&refused)
    }
}

impl std::error::Error for Refused {}

/// Why the node at `base` could not be reached, if that is what `e` means:
/// the ID of a `reason-…` text.
fn unreachable(e: &anyhow::Error) -> Option<&'static str> {
    use std::io::ErrorKind;
    e.chain().find_map(|c| {
        let io = match c.downcast_ref::<ureq::Error>() {
            Some(ureq::Error::Io(io)) => io,
            Some(ureq::Error::HostNotFound) => return Some("reason-host-not-found"),
            Some(ureq::Error::ConnectionFailed) => return Some("reason-connection-failed"),
            Some(ureq::Error::Timeout(_)) => return Some("reason-timed-out"),
            Some(_) => return None,
            // From the event stream.
            None => c.downcast_ref::<std::io::Error>()?,
        };
        let reason = match io.kind() {
            ErrorKind::ConnectionRefused => "reason-connection-refused",
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => "reason-connection-reset",
            ErrorKind::HostUnreachable => "reason-host-unreachable",
            ErrorKind::NetworkUnreachable => "reason-network-unreachable",
            ErrorKind::TimedOut => "reason-timed-out",
            ErrorKind::AddrNotAvailable => "reason-address-not-available",
            // Name resolution has no kind of its own.
            _ if io.to_string().contains("lookup address") => "reason-host-not-found",
            ErrorKind::NotFound => "reason-host-not-found",
            _ => return None,
        };
        Some(reason)
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
        Some("reason-connection-refused") if is_local(base) => format!(
            "{}\n  {}\n",
            p.mark(Level::Bad, &t!("not-running")),
            p.dim(&t!("not-running-hint")),
        ),
        Some(reason) => {
            let reason = i18n::texts().get(reason).unwrap_or_default();
            format!(
                "{}\n  {}\n",
                p.mark(Level::Bad, &t!("no-node-at", base = base, reason = reason)),
                p.dim(&t!("no-node-at-hint")),
            )
        }
        None => format!("{}\n", p.mark(Level::Bad, &format!("{e:#}"))),
    }
}

/// How long to wait for a node to answer at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// An agent that leaves error statuses to [`reply`] and gives up quickly on
/// a node that does not answer.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
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
        .with_context(|| t!("querying", url = url))
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
        [] => anyhow::bail!(t!("no-output", query = query)),
        _ => {
            let outputs = found
                .iter()
                .map(|o| format!("{:?}", o.description))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(t!("output-ambiguous", query = query, outputs = outputs))
        }
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
    let s = serde_json::from_str(&raw)
        .with_context(|| t!("reading-from", what = "status", url = &url))?;
    Ok((raw, s))
}

/// Shows the status as the node pushes it, at most every `interval`, until
/// interrupted. On a terminal each frame replaces the last; otherwise frames
/// follow each other. With JSON every sample is one line. Errors are shown
/// and the connection tried again, so the node may restart meanwhile.
fn watch(base: &str, interval: Duration, json: bool) -> Result<()> {
    let paint = Paint::detect();
    let mut screen = Screen {
        out: std::io::stdout().lock(),
        tty: std::io::stdout().is_terminal(),
        json,
        first: true,
    };
    loop {
        let e = follow(base, interval, paint, &mut screen).unwrap_err();
        // Output that can't be written ends the watch.
        if let Some(Unwritable(e)) = e.downcast_ref::<Unwritable>() {
            return Err(std::io::Error::new(e.kind(), e.to_string()).into());
        }
        screen
            .show(&render_error(&e, base, paint))
            .map_err(|Unwritable(e)| e)?;
        std::thread::sleep(interval);
    }
}

/// Where watched frames go.
struct Screen<'a> {
    out: std::io::StdoutLock<'a>,
    tty: bool,
    json: bool,
    first: bool,
}

/// The watched frames could not be written.
#[derive(Debug)]
struct Unwritable(std::io::Error);

impl std::fmt::Display for Unwritable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Unwritable {}

impl Screen<'_> {
    fn show(&mut self, frame: &str) -> Result<(), Unwritable> {
        self.write(frame).map_err(Unwritable)
    }

    fn write(&mut self, frame: &str) -> std::io::Result<()> {
        if self.json {
            self.out.write_all(frame.as_bytes())?;
        } else if self.tty {
            // Home, then overwrite each line and clear the rest, which
            // flickers less than clearing the screen first.
            let home = if self.first {
                "\x1b[2J\x1b[H"
            } else {
                "\x1b[H"
            };
            let mut f = String::from(home);
            for line in frame.lines() {
                let _ = writeln!(f, "{line}\x1b[K");
            }
            f.push_str("\x1b[J");
            self.out.write_all(f.as_bytes())?;
        } else {
            if !self.first {
                self.out.write_all(b"\n")?;
            }
            self.out.write_all(frame.as_bytes())?;
        }
        self.first = false;
        self.out.flush()
    }
}

/// Shows the status pushed over one connection, until it fails. Errors
/// writing the output come back as [`Unwritable`].
fn follow(base: &str, interval: Duration, paint: Paint, screen: &mut Screen) -> Result<()> {
    let mut socket = events(base, "status", interval)?;
    // Rates need two samples: the one last shown (or the first) and a newer
    // one. No newer one means nothing changed.
    let mut prev: Option<(Instant, Status)> = None;
    let mut fresh: Option<(Instant, Status)> = None;
    let mut shown: Option<Instant> = None;
    loop {
        let quiet = match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let event: Event =
                    serde_json::from_str(&text).with_context(|| t!("reading", what = "event"))?;
                if screen.json {
                    screen.show(&format!("{}\n", event.data))?;
                    continue;
                }
                let s: Status = serde_json::from_value(event.data)
                    .with_context(|| t!("reading", what = "status"))?;
                let sample = Some((Instant::now(), s));
                if prev.is_none() {
                    prev = sample;
                } else {
                    fresh = sample;
                }
                false
            }
            Ok(tungstenite::Message::Close(_)) => anyhow::bail!(t!("node-closed")),
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                true
            }
            Err(
                tungstenite::Error::ConnectionClosed
                | tungstenite::Error::AlreadyClosed
                | tungstenite::Error::Protocol(_),
            ) => anyhow::bail!(t!("node-closed")),
            Err(e) => return Err(anyhow::Error::new(e).context(t!("reading", what = "events"))),
        };
        if screen.json {
            continue;
        }
        let now = Instant::now();
        let due = shown.is_none_or(|t| now - t >= interval);
        let frame = match (&prev, &fresh) {
            (Some((at, p)), Some((t, s))) if due => {
                let secs = (*t - *at).as_secs_f64();
                render(&s.since(p), paint, Counts::Rate(secs))
            }
            // Nothing changed for a while: no counter moved.
            (Some((_, p)), None) if quiet && due => render(&p.since(p), paint, Counts::Rate(1.0)),
            _ => continue,
        };
        screen.show(&frame)?;
        shown = Some(now);
        if fresh.is_some() {
            prev = fresh.take();
        }
    }
}

/// A message on the event stream.
#[derive(Deserialize)]
struct Event {
    data: serde_json::Value,
}

/// Opens the node's event stream for `topics`; reading it times out after
/// `timeout` without a message.
fn events(
    base: &str,
    topics: &str,
    timeout: Duration,
) -> Result<tungstenite::WebSocket<std::net::TcpStream>> {
    use std::net::{TcpStream, ToSocketAddrs};
    let uri: ureq::http::Uri = base
        .parse()
        .with_context(|| t!("bad-node-url", base = base))?;
    anyhow::ensure!(
        uri.scheme_str() == Some("http"),
        t!("watch-needs-http", base = base)
    );
    let host = uri.host().unwrap_or_default();
    let port = uri.port_u16().unwrap_or(80);
    let addr = (host.trim_start_matches('[').trim_end_matches(']'), port)
        .to_socket_addrs()
        .and_then(|mut a| a.next().ok_or_else(|| std::io::ErrorKind::NotFound.into()))
        .with_context(|| t!("looking-up", host = host))?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .with_context(|| t!("connecting", base = base))?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    let url = format!("ws://{host}:{port}/x-orpheus/v1/events?topics={topics}");
    let (socket, _) = tungstenite::client(url.as_str(), stream)
        .map_err(|e| match e {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r)) => Refused {
                status: r.status().as_u16(),
                message: r
                    .body()
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).trim().to_owned())
                    .unwrap_or_default(),
            }
            .into(),
            tungstenite::HandshakeError::Failure(e) => anyhow::Error::new(e),
            tungstenite::HandshakeError::Interrupted(_) => anyhow::anyhow!(t!("reason-timed-out")),
        })
        .with_context(|| t!("opening", url = &url))?;
    socket.get_ref().set_read_timeout(Some(timeout))?;
    Ok(socket)
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
    /// The number as shown, and as it chooses the plural of the name
    /// after it.
    fn show(self, n: u64) -> (String, f64) {
        match self {
            Counts::Total => (n.to_string(), n as f64),
            Counts::Rate(secs) => {
                let r = n as f64 / secs;
                let text = if r >= 10.0 || r.fract() == 0.0 {
                    format!("{r:.0}")
                } else {
                    num(format!("{r:.1}"))
                };
                (text, r)
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

/// The keys of the status, by their text IDs.
const KEYS: [&str; 23] = [
    "key-error",
    "key-interface",
    "key-identity",
    "key-state",
    "key-grandmaster",
    "key-offset",
    "key-delay",
    "key-frequency",
    "key-steps",
    "key-listen",
    "key-channels",
    "key-output",
    "key-mix",
    "key-priority",
    "key-link-offset",
    "key-level",
    "key-margin",
    "key-jitter",
    "key-packets",
    "key-ignored",
    "key-buffer",
    "key-asrc",
    "key-problems",
];

/// Width of the key column, so that values line up under section
/// headlines: that of the longest key in the user's language.
fn key_width() -> usize {
    static WIDTH: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        let texts = i18n::texts();
        KEYS.iter()
            .filter_map(|id| texts.get(id))
            .map(|k| k.chars().count())
            .max()
            .unwrap_or(0)
    });
    *WIDTH
}

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
    let title = format!("{title:<w$}", w = key_width() + 4);
    let line = match mark {
        Some(m) => format!("{}{m}", p.bold(&title)),
        None => p.bold(title.trim_end()),
    };
    let _ = writeln!(o, "\n{line}");
}

/// A row of the status: the key by its text ID, one of [`KEYS`].
fn row(o: &mut String, p: Paint, key: &str, value: &str) {
    let key = i18n::texts().get(key).unwrap_or_else(|| key.to_owned());
    let _ = writeln!(
        o,
        "  {}  {value}",
        p.dim(&format!("{key:<w$}", w = key_width()))
    );
}

fn us(ns: Option<i64>) -> String {
    ns.map_or("-".into(), |n| {
        format!("{} µs", num(format!("{:.1}", n as f64 / 1000.0)))
    })
}

/// The non-zero counters, highlighted, or "none". Each is the ID of a
/// `count-…` text.
fn problems(p: Paint, c: Counts, counts: &[(u64, &'static str)]) -> String {
    let found: Vec<String> = counts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|&(n, id)| {
            let (shown, count) = c.show(n);
            let text = orpheus_i18n::Text::new(id)
                .arg("n", shown)
                .arg("count", orpheus_i18n::Arg::Count(count));
            format!("{}{}", i18n::texts().text(&text), c.unit())
        })
        .collect();
    if found.is_empty() {
        t!("none")
    } else {
        p.level(Level::Warn, &found.join(", "))
    }
}

fn buffer_counts(b: &Buffer) -> [(u64, &'static str); 3] {
    [
        (b.underruns, "count-underrun"),
        (b.overruns, "count-overrun"),
        (b.resyncs, "count-resync"),
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
        let mark = p.mark(Level::Bad, &t!("no-audio"));
        heading(&mut o, p, &t!("heading-audio"), Some(mark));
    }
    if !s.warnings.is_empty() {
        heading(&mut o, p, &t!("heading-warnings"), None);
        for w in &s.warnings {
            let _ = writeln!(o, "  {}", p.mark(Level::Warn, &w.message));
        }
    }
    match &s.clock {
        Some(ClockInfo { ptp: Some(t), .. }) => clock(&mut o, p, t),
        Some(c) => {
            let source = match c.source.as_str() {
                "system" => t!("system-clock"),
                other => other.to_owned(),
            };
            let mark = p.mark(Level::Warn, &source);
            heading(&mut o, p, &t!("heading-clock"), Some(mark));
        }
        None => {}
    }
    for r in &s.receivers {
        receiver(&mut o, p, c, r, s.accept.as_deref());
    }
    if let Some(l) = &s.loopback {
        heading(&mut o, p, &t!("heading-loopback"), None);
        row(&mut o, p, "key-buffer", &t!("frames", n = l.level_frames));
        row(
            &mut o,
            p,
            "key-problems",
            &problems(p, c, &buffer_counts(l)),
        );
    }
    senders(&mut o, p, c, &s.routes);
    o
}

fn clock(o: &mut String, p: Paint, t: &Ptp) {
    if !t.running {
        let mark = if t.holdover {
            p.mark(Level::Bad, &t!("ptp-holding-over"))
        } else {
            p.mark(Level::Bad, &t!("ptp-not-running"))
        };
        heading(o, p, &t!("heading-clock"), Some(mark));
        row(o, p, "key-error", t.error.as_deref().unwrap_or("-"));
        if !t.interface.is_empty() {
            row(o, p, "key-interface", &t.interface);
        }
        row(o, p, "key-frequency", &ppm(t.frequency_ppm, 2));
        row(o, p, "key-steps", &t.steps.to_string());
        return;
    }
    let state = t!("ptp-state", state = &t.state);
    let mark = match t.state.as_str() {
        "faulty" | "disabled" => p.mark(Level::Bad, &t!("ptp-heading-state", state = &state)),
        _ if t.settled => p.mark(Level::Good, &t!("ptp-settled")),
        _ => p.mark(Level::Warn, &t!("ptp-settling")),
    };
    heading(o, p, &t!("heading-clock"), Some(mark));
    let transport = match t.transport.as_str() {
        "ipv4" => "IPv4",
        "ipv6" => "IPv6",
        other => other,
    };
    row(
        o,
        p,
        "key-interface",
        &format!("{} ({transport})", t.interface),
    );
    row(o, p, "key-identity", &t.identity);
    row(o, p, "key-state", &state);
    let gm = if t.grandmaster == t.identity {
        t!("this-node")
    } else {
        t.grandmaster.clone()
    };
    row(o, p, "key-grandmaster", &gm);
    row(o, p, "key-offset", &us(t.offset_ns));
    row(o, p, "key-delay", &us(t.mean_delay_ns));
    row(o, p, "key-frequency", &ppm(t.frequency_ppm, 2));
    // One or two steps after a start are normal; more mean the clock jumps.
    let steps = t.steps.to_string();
    let steps = if t.steps > 2 {
        p.level(Level::Warn, &steps)
    } else {
        steps
    };
    row(o, p, "key-steps", &steps);
}

/// A rate deviation, `+1.25 ppm`.
fn ppm(ppm: f64, places: usize) -> String {
    format!("{} ppm", num(format!("{ppm:+.places$}")))
}

fn receiver(o: &mut String, p: Paint, c: Counts, r: &Receiver, accept: Option<&str>) {
    let title = t!("receiver-title", id = r.id);
    let Some(st) = &r.stream else {
        let mark = match &r.sender {
            Some(from) => p.mark(Level::Off, &t!("connected-off", from = from)),
            None => p.mark(Level::Off, &t!("free-port", port = r.port)),
        };
        heading(o, p, &title, Some(mark));
        return;
    };
    let mark = match (st.ssrc, &r.sender) {
        (Some(_), Some(from)) => match r.state.as_deref() {
            Some("muted") => p.mark(Level::Off, &t!("receiving-muted", from = from)),
            Some("ducked") => p.mark(Level::Good, &t!("receiving-ducked", from = from)),
            _ => p.mark(Level::Good, &t!("receiving-from", from = from)),
        },
        (Some(ssrc), None) => p.mark(
            Level::Good,
            &t!("receiving-ssrc", ssrc = format!("{ssrc:08x}")),
        ),
        (None, Some(from)) => p.mark(Level::Off, &t!("connected-idle", from = from)),
        (None, None) if accept == Some("any") => p.mark(Level::Off, &t!("free-any")),
        (None, None) => p.mark(Level::Off, &t!("idle")),
    };
    heading(o, p, &title, Some(mark));
    row(o, p, "key-listen", &st.listen);
    if let Some(layout) = &r.layout {
        row(o, p, "key-channels", &channels(layout));
    }
    if let Some(parked) = &r.parked {
        let why = match (parked.why.as_str(), &parked.description) {
            ("lost", Some(output)) => t!("parked-lost", output = output),
            ("unwired", _) => t!("parked-unwired"),
            _ => t!("parked"),
        };
        row(o, p, "key-output", &p.mark(Level::Off, &why));
    } else if r.held {
        row(o, p, "key-output", &p.mark(Level::Bad, &t!("held")));
    } else if r.wired {
        row(o, p, "key-output", &t!("on-wires"));
    } else if let Some(output) = &r.output {
        let output = match &r.plays_on {
            Some(on) => format!("{output} ({on})"),
            None => output.clone(),
        };
        row(o, p, "key-output", &output);
        if let Some(mix) = &r.mix {
            let text = mix_text(mix, r.layout.as_ref(), r.output_channels.as_ref(), &[]);
            row(o, p, "key-mix", &text);
        }
    }
    if r.priority.as_deref() == Some("high") {
        row(o, p, "key-priority", &t!("priority-high"));
    }
    row(
        o,
        p,
        "key-link-offset",
        &format!("{} ms", st.link_offset_ms),
    );
    let level = st.peak_dbfs.map_or_else(
        || t!("silence"),
        |db| format!("{} dBFS", num(format!("{db:.1}"))),
    );
    row(o, p, "key-level", &level);
    let margin = match st.margin_us {
        Some(m) => {
            let text = ms(m);
            match m {
                ..=0 => p.level(Level::Bad, &text),
                1..1000 => p.level(Level::Warn, &text),
                _ => text,
            }
        }
        None => "-".into(),
    };
    row(o, p, "key-margin", &margin);
    if let Some(j) = st.jitter_us {
        let text = ms(j);
        // As orpheusd's health check warns.
        let text = if j > 3000 {
            p.level(Level::Warn, &text)
        } else {
            text
        };
        row(o, p, "key-jitter", &text);
    }
    row(o, p, "key-packets", &c.packets(st.packets));
    if st.ignored > 0 {
        // From other sources: expected while connected to one sender.
        row(o, p, "key-ignored", &c.packets(st.ignored));
    }
    let frames = t!("frames", n = st.buffer.level_frames);
    row(o, p, "key-buffer", &frames);
    row(o, p, "key-asrc", &ppm(st.asrc_ppm, 1));
    let mut counts = vec![
        (st.lost, "count-lost"),
        (st.late, "count-late"),
        (st.early, "count-early"),
        (st.malformed, "count-malformed"),
        (st.realigns, "count-realign"),
    ];
    counts.extend(buffer_counts(&st.buffer));
    row(o, p, "key-problems", &problems(p, c, &counts));
}

/// Microseconds as milliseconds, `1.25 ms`.
fn ms(us: i64) -> String {
    format!("{} ms", num(format!("{:.2}", us as f64 / 1000.0)))
}

/// The channels of a layout as `Left (L), Right (R)`.
fn channels(layout: &Layout) -> String {
    let names: Vec<String> = layout
        .symbols()
        .enumerate()
        .map(|(i, s)| format!("{} ({s})", layout.label(i)))
        .collect();
    names.join(", ")
}

fn senders(o: &mut String, p: Paint, c: Counts, senders: &[Sender]) {
    if senders.is_empty() {
        let mark = p.mark(Level::Off, &t!("none"));
        heading(o, p, &t!("heading-routes"), Some(mark));
        return;
    }
    heading(o, p, &t!("heading-routes"), None);
    let header = [
        t!("col-id"),
        t!("col-name"),
        t!("col-peer"),
        t!("col-packets"),
        t!("col-buffer"),
        t!("col-asrc"),
        t!("col-problems"),
    ];
    let rows: Vec<[String; 7]> = senders
        .iter()
        .map(|t| {
            let mut counts = vec![
                (t.send_errors, "count-send-error"),
                (t.realigns, "count-realign"),
            ];
            counts.extend(buffer_counts(&t.buffer));
            [
                t.id.clone(),
                t.name.clone(),
                peer_column(t),
                c.packets(t.packets),
                t!("frames", n = t.buffer.level_frames),
                ppm(t.asrc_ppm, 1),
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
        Some("disconnected") => notes.push(t!("peer-disconnected")),
        Some("busy") => notes.push(t!("peer-busy")),
        Some("incompatible") => notes.push(t!("peer-incompatible")),
        Some("offline") => notes.push(t!("peer-offline")),
        Some("unreachable") => notes.push(t!("peer-unreachable")),
        Some("connecting") => notes.push(t!("peer-connecting")),
        Some("idle") => notes.push(t!("peer-idle")),
        _ if !t.enabled && !t.hidden => notes.push(t!("peer-off")),
        _ => {}
    }
    match t.receiver.as_ref().and_then(|r| r.parked.as_deref()) {
        Some("lost") => notes.push(t!("peer-output-gone")),
        Some("unwired") => notes.push(t!("peer-unwired")),
        Some(why) => notes.push(why.to_owned()),
        None => {}
    }
    if t.hidden {
        notes.push(t!("peer-sink-hidden"));
    }
    match t.receiver.as_ref().and_then(|r| r.policy.as_deref()) {
        Some("muted") => notes.push(t!("peer-muted")),
        Some("ducked") => notes.push(t!("peer-ducked")),
        Some(other) => notes.push(other.to_owned()),
        None => {}
    }
    if t.priority.as_deref() == Some("high") {
        notes.push(t!("peer-high-priority"));
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
    header: [String; N],
    right: [bool; N],
    rows: &[[String; N]],
) {
    let mut width = header.each_ref().map(|h| h.chars().count());
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
    let _ = writeln!(
        o,
        "{}",
        p.dim(line(header.each_ref().map(String::as_str)).trim_end())
    );
    for r in rows {
        let _ = writeln!(o, "{}", line(r.each_ref().map(String::as_str)).trim_end());
    }
}

/// A node's outputs.
fn render_outputs(outputs: &[Output], p: Paint) -> String {
    let mut o = String::new();
    if outputs.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, &t!("no-outputs")));
        return o;
    }
    let rows: Vec<[String; 5]> = outputs
        .iter()
        .map(|out| {
            let kind = match &out.overridden {
                Some(_) => t!("type-set", type = &out.kind),
                None => out.kind.clone(),
            };
            let mut notes = Vec::new();
            if let Some(c) = &out.created {
                let device = outputs
                    .iter()
                    .find(|o| o.name == c.device)
                    .map_or(c.device.as_str(), |o| o.description.as_str());
                let channels: Vec<String> = c.channels.iter().map(usize::to_string).collect();
                let channels = channels.join(", ");
                notes.push(if outputs.iter().any(|o| o.name == c.device) {
                    t!("created-on", device = device, channels = channels)
                } else {
                    t!("created-on-missing", device = device, channels = channels)
                });
            }
            if !out.receivers.is_empty() {
                let ids: Vec<String> = out.receivers.iter().map(u32::to_string).collect();
                notes.push(t!(
                    "plays-receivers",
                    count = ids.len(),
                    ids = ids.join(", ")
                ));
            }
            if out.default {
                notes.push(t!("default"));
            }
            [
                out.description.clone(),
                kind,
                short(&out.channels),
                out.name.clone(),
                notes.join(", "),
            ]
        })
        .collect();
    let header = [
        t!("col-output"),
        t!("col-type"),
        t!("col-channels"),
        t!("col-name"),
        String::new(),
    ];
    table(&mut o, p, header, [false; 5], &rows);
    o
}

/// A layout in short, `ST` or `L R C`; `-` for none known yet.
fn short(layout: &Layout) -> String {
    if layout.is_empty() {
        "-".into()
    } else {
        layout.to_string()
    }
}

/// A node's inputs.
fn render_inputs(inputs: &[InputEntry], p: Paint) -> String {
    let mut o = String::new();
    if inputs.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, &t!("no-inputs")));
        return o;
    }
    let rows: Vec<[String; 4]> = inputs
        .iter()
        .map(|i| {
            [
                i.description.clone(),
                t!("input-kind", kind = &i.kind),
                short(&i.channels),
                i.name.clone(),
            ]
        })
        .collect();
    let header = [
        t!("col-input"),
        t!("col-kind"),
        t!("col-channels"),
        t!("col-name"),
    ];
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
            let source = v["sources"][section][key].as_str().unwrap_or("default");
            let from = t!("setting-from", from = source);
            let from = if source == "default" {
                p.dim(&from)
            } else {
                from
            };
            rows.push([format!("{section}.{key}"), value, from]);
        }
    }
    let mut o = String::new();
    let header = [t!("col-setting"), t!("col-value"), t!("col-from")];
    table(&mut o, p, header, [false; 3], &rows);
    o
}

/// The nodes a node found on the network.
fn render_nodes(nodes: &[Node], p: Paint) -> String {
    let mut o = String::new();
    if nodes.is_empty() {
        let _ = writeln!(o, "{}", p.mark(Level::Off, &t!("no-nodes")));
        return o;
    }
    let rows: Vec<[String; 6]> = nodes
        .iter()
        .map(|n| {
            let state = match (n.online, n.pinned) {
                (true, false) => p.mark(Level::Good, &t!("online")),
                (true, true) => p.mark(Level::Good, &t!("online-pinned")),
                (false, false) => p.mark(Level::Off, &t!("offline")),
                (false, true) => p.mark(Level::Off, &t!("offline-pinned")),
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
                    Some("wired") => t!("wired"),
                    Some("wifi") => p.level(Level::Warn, &t!("wifi")),
                    _ => "-".into(),
                },
                state,
            ]
        })
        .collect();
    let header = [
        t!("col-name"),
        t!("col-id"),
        t!("col-address"),
        t!("col-version"),
        t!("col-link"),
        t!("col-state"),
    ];
    table(&mut o, p, header, [false; 6], &rows);
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
        "id": "B73AQ0P0", "name": "desk", "version": "0.0.1", "audio": true,
        "clock": {"source": "ptp", "ptp": {
            "running": true, "holdover": false,
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
            "level_frames": 1140}, "peak_dbfs": -12.04},
            "layout": [{"symbol": "L"}, {"symbol": "R", "label": "Rechts"}]},
            {"id": 1, "port": 5006}],
        "routes": [{"id": "K7QM3XRA", "name": "Kitchen", "peer": "192.168.1.20:5004", "packets": 120000,
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
  channels     Left (L), Rechts (R)
  link offset  10 ms
  level        -12.0 dBFS
  margin       8.71 ms
  packets      120000
  buffer       1140 frames
  ASRC         -3.2 ppm
  problems     1 lost, 3 late

Receiver 1     ○ free, port 5006

Routes
  ID        NAME     PEER               PACKETS       BUFFER      ASRC  PROBLEMS
  K7QM3XRA  Kitchen  192.168.1.20:5004   120000  1120 frames  +8.9 ppm  2 underruns
";
        assert_eq!(out, expected, "\n{out}");
    }

    #[test]
    fn renders_routes() {
        let routes: Vec<RouteEntry> = serde_json::from_str(
            r#"[{"id": "K7QM3XRA", "name": "Kitchen",
                 "priority": "normal", "peer": "192.168.1.20:5004", "active": true,
                 "layout": [{"symbol": "L", "label": "Left"}, {"symbol": "R", "label": "Right"}]},
                {"id": "9F2D4C71", "name": "Kitchen",
                 "priority": "high", "peer": "192.168.1.20:5006", "active": false,
                 "last_used": 1000,
                 "layout": [{"symbol": "L"}, {"symbol": "R"}, {"symbol": "C"}, {"symbol": "LFE"},
                            {"symbol": "Ls"}, {"symbol": "Rs"}]},
                {"id": "M2R8D0C4", "name": "Bath",
                 "priority": "normal", "peer": "192.168.1.21:5004", "active": true,
                 "state": "idle"}]"#,
        )
        .unwrap();
        let out = render_routes(&routes, Paint(false), 1000 + 3 * 86_400);
        assert_eq!(
            out,
            "  NAME     ID        PEER               CHANNELS  PRIORITY  STATE\n  \
             Kitchen  K7QM3XRA  192.168.1.20:5004  ST        normal    ● connected\n  \
             Kitchen  9F2D4C71  192.168.1.20:5006  51        high      ○ remembered, last used 3 days ago\n  \
             Bath     M2R8D0C4  192.168.1.21:5004  -         normal    ○ idle, claims a receiver when audio plays\n"
        );
    }

    #[test]
    fn renders_sources() {
        let sources: Vec<SourceEntry> = serde_json::from_str(
            r#"[{"id": "6S0AGTEC", "name": "Desk", "key": "orpheus.source.x",
                 "output": "test.two", "volume": {"channels": [0.125, 0.1], "mute": false},
                 "receiver": 0, "last_used": 1000},
                {"id": "V1MVRAV7", "name": "Phone", "key": "orpheus.from.192.168.1.30",
                 "volume": {"channels": [1.0], "mute": true},
                 "layout": [{"symbol": "M1"}, {"symbol": "M2"}], "last_used": 1000}]"#,
        )
        .unwrap();
        let outputs: Vec<Output> = serde_json::from_str(
            r#"[{"name": "test.two", "description": "Test two", "type": "hifi",
                 "override": null, "default": false, "channels": []}]"#,
        )
        .unwrap();
        let out = render_sources(&sources, &outputs, Paint(false), 1000 + 86_400);
        assert_eq!(
            out,
            "  NAME   ID        OUTPUT    VOLUME  LAYOUT   STATE\n  \
             Desk   6S0AGTEC  Test two     50%  as sent  ● playing on receiver 0\n  \
             Phone  V1MVRAV7  default    muted  DM       ○ last played yesterday\n"
        );
    }

    #[test]
    fn patch_shows_mixes() {
        let st = r#"[{"symbol": "L"}, {"symbol": "R"}]"#;
        let six = r#"[{"symbol": "L"}, {"symbol": "R"}, {"symbol": "C"},
                      {"symbol": "LFE"}, {"symbol": "Ls"}, {"symbol": "Rs"}]"#;
        let link = |from: usize, to: usize, folded: bool| {
            let gain = if folded { -3 } else { 0 };
            format!(
                r#"{{"from": {from}, "to": {to}, "linked": true, "gain_db": {gain}, "folded": {folded}}}"#
            )
        };
        let links = [
            link(1, 1, false),
            link(2, 2, false),
            link(3, 1, true),
            link(3, 2, true),
            link(5, 1, true),
            link(6, 2, true),
        ]
        .join(",");
        let wires: Vec<WireEntry> = serde_json::from_str(&format!(
            r#"[{{"source": "6S0AGTEC", "from_name": "Film", "output": "tv",
                  "output_name": "TV", "links": [{links}], "mix": "downmix",
                  "dropped": [4], "from_channels": {six}, "output_channels": {st}}}]"#
        ))
        .unwrap();
        assert_eq!(
            render_patch(&wires, None, Paint(false)),
            "TV (tv)\n  \
             L  ← Film: L, Film: C⤵ -3 dB, Film: Ls⤵ -3 dB\n  \
             R  ← Film: R, Film: C⤵ -3 dB, Film: Rs⤵ -3 dB\n  \
             ⤵ Film: downmix 51 → ST, LFE left out\n"
        );
        assert_eq!(
            render_wire_set(&wires[0], Paint(false)),
            "Film → TV: L=L R=R L=C⤵@-3 R=C⤵@-3 L=Ls⤵@-3 R=Rs⤵@-3 (downmix 51 → ST, LFE left out)\n"
        );
    }

    #[test]
    fn patch_shows_levels() {
        let wires: Vec<WireEntry> = serde_json::from_str(
            r#"[{"input": "mic", "from_name": "Mic", "output": "tv", "output_name": "TV",
                 "links": [
                     {"from": 1, "to": 1, "linked": true},
                     {"from": 1, "to": 2, "linked": true, "gain_db": -6}]}]"#,
        )
        .unwrap();
        let levels: Levels = serde_json::from_str(
            r#"{"outputs": [{"output": "tv", "channels": [{"peak_db": 0.4}, {}],
                             "clipped_secs_ago": 2.3}],
                "wires": [{"input": "mic", "output": "tv", "links": [
                    {"from": 1, "to": 1, "peak_db": -9.04, "rms_db": -12},
                    {"from": 1, "to": 2}]}]}"#,
        )
        .unwrap();
        assert_eq!(
            render_patch(&wires, Some(&levels), Paint(false)),
            "TV (tv)  clipped 2 s ago\n  \
             1    0.4 dBFS  ← Mic: 1 [-9.0 dBFS]\n  \
             2      silent  ← Mic: 1 -6 dB [silent]\n"
        );
        let unmeasured = Levels::default();
        assert_eq!(
            render_patch(&wires, Some(&unmeasured), Paint(false)),
            "TV (tv)\n  1           —  ← Mic: 1 [silent]\n  2           —  ← Mic: 1 -6 dB [silent]\n"
        );
    }

    #[test]
    fn wire_gains() {
        let wires: Vec<WireEntry> = serde_json::from_str(
            r#"[{"input": "mic", "from_name": "Mic", "output": "tv", "output_name": "TV",
                 "gain_db": -6, "links": [
                     {"from": 1, "to": 1, "linked": true, "gain_db": -6},
                     {"from": 1, "to": 2, "linked": true, "gain_db": 0}]}]"#,
        )
        .unwrap();
        assert_eq!(
            render_wire_set(&wires[0], Paint(false)),
            "Mic → TV (-6 dB): 1=1 2=1@+6\n"
        );
        assert_eq!(
            render_patch(&wires, None, Paint(false)),
            "TV (tv)\n  1  ← Mic: 1 -6 dB\n  2  ← Mic: 1\n"
        );
        let mut muted = wires;
        muted[0].mute = true;
        assert_eq!(
            render_wire_set(&muted[0], Paint(false)),
            "Mic → TV (-6 dB, muted): 1=1 2=1@+6\n"
        );
        assert!(render_patch(&muted, None, Paint(false)).contains("← Mic: 1 muted\n"));
        assert_eq!(
            parse_pair("L=C@-3").unwrap(),
            serde_json::json!({"to": "L", "from": "C", "gain_db": -3.0})
        );
        assert_eq!(
            parse_pair("2=1@+1.5 dB").unwrap(),
            serde_json::json!({"to": 2, "from": 1, "gain_db": 1.5})
        );
        assert!(parse_pair("L=C@loud").is_err());
    }

    #[test]
    fn watch_shows_rates() {
        let prev: Status = serde_json::from_str(STATUS).unwrap();
        let mut s = prev.clone();
        let r = s.receivers[0].stream.as_mut().unwrap();
        r.packets += 2000;
        r.late += 3;
        s.routes[0].packets += 2000;
        s.routes[0].buffer.underruns += 2;
        let mut new = s.routes[0].clone();
        new.id = "9F2D4C71".into();
        s.routes.push(new);
        let out = render(&s.since(&prev), Paint(false), Counts::Rate(2.0));
        assert!(out.contains("  packets      1000/s\n"), "{out}");
        assert!(out.contains("  problems     1.5 late/s\n"), "{out}");
        assert!(
            out.contains("1000/s  1120 frames  +8.9 ppm  1 underrun/s\n"),
            "{out}"
        );
        // A route that appeared since the last sample counts from now.
        assert!(
            out.contains(
                "  9F2D4C71  Kitchen  192.168.1.20:5004      0/s  1120 frames  +8.9 ppm  none\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn shows_ports_that_are_off() {
        let mut s: Status = serde_json::from_str(STATUS).unwrap();
        s.receivers[0].stream.as_mut().unwrap().ignored = 42;
        s.receivers[1].sender = Some("Laptop".into());
        s.routes[0].enabled = false;
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

        s.routes[0].receiver = Some(PeerLink {
            state: "disconnected".into(),
            policy: None,
            parked: None,
        });
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("  Kitchen  192.168.1.20:5004 (disconnected, paused)  "),
            "{out}"
        );
    }

    #[test]
    fn shows_why_the_peer_plays_nowhere() {
        let mut s: Status = serde_json::from_str(STATUS).unwrap();
        s.routes[0].receiver = Some(PeerLink {
            state: "connected".into(),
            policy: None,
            parked: Some("lost".into()),
        });
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("  Kitchen  192.168.1.20:5004 (its output there is gone)  "),
            "{out}"
        );
        s.routes[0].receiver = Some(PeerLink {
            state: "offline".into(),
            policy: None,
            parked: None,
        });
        s.routes[0].hidden = true;
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("  Kitchen  192.168.1.20:5004 (peer offline, sink hidden)  "),
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
        s.routes[0].receiver = Some(PeerLink {
            state: "connected".into(),
            policy: Some("muted".into()),
            parked: None,
        });
        s.routes[0].priority = Some("high".into());
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
        s.receivers[0].mix = Some("upmix".into());
        s.receivers[0].output_channels = Some(Layout::parse("51").unwrap());
        let out = render(&s, Paint(false), Counts::Total);
        assert!(out.contains("  mix          upmix ST → 51\n"), "{out}");
        s.receivers[0].mix = None;
        s.receivers[0].parked = Some(Parked {
            why: "lost".into(),
            description: Some("Kitchen speaker".into()),
        });
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("  output       ○ parked: Kitchen speaker is gone\n"),
            "{out}"
        );
        s.receivers[0].parked = None;
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
            message: "route Kitchen: Kitchen is on Wi-Fi; expect dropouts".into(),
        });
        s.receivers[0].stream.as_mut().unwrap().jitter_us = Some(4210);
        let out = render(&s, Paint(false), Counts::Total);
        assert!(
            out.contains("\nWarnings\n  ● route Kitchen: Kitchen is on Wi-Fi; expect dropouts\n"),
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
                 "type":"hifi","detected":"computer","override":"hifi","default":true,"receivers":[0,2],
                 "channels":[{"symbol":"L"},{"symbol":"R"}]},
                {"name":"orpheus.send.0","description":"Kitchen","type":"speaker",
                 "detected":"speaker","default":false,"receivers":[],
                 "channels":[{"symbol":"L"},{"symbol":"R"}]},
                {"name":"orpheus.output.desk","description":"Desk","type":"generic",
                 "detected":"generic","default":false,"receivers":[],
                 "channels":[{"symbol":"M1"}],
                 "created":{"device":"alsa_output.pci-0000_00_1f.3.analog-stereo","channels":[2]}}
            ]"#,
        )
        .unwrap();
        let expected = "  OUTPUT          TYPE        CHANNELS  NAME
  Built-in Audio  hifi (set)  ST        alsa_output.pci-0000_00_1f.3.analog-stereo  receivers 0, 2, default
  Kitchen         speaker     ST        orpheus.send.0
  Desk            generic     M         orpheus.output.desk                         on Built-in Audio 2
";
        assert_eq!(render_outputs(&outputs, Paint(false)), expected);
        assert_eq!(
            find_output(&outputs, "kitchen").unwrap().name,
            "orpheus.send.0"
        );
        assert_eq!(
            find_output(&outputs, "pci").unwrap().description,
            "Built-in Audio"
        );
        assert!(find_output(&outputs, "e").is_err());
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

    #[test]
    fn clock_holdover() {
        let t = Ptp {
            running: false,
            error: Some("interface wlan0 is down".into()),
            holdover: true,
            interface: "wlan0".into(),
            transport: "ipv4".into(),
            identity: "4cb04a.fffe.50a582".into(),
            state: "faulty".into(),
            grandmaster: "9c6b00.fffe.1a2b3c".into(),
            offset_ns: None,
            mean_delay_ns: None,
            frequency_ppm: 12.31,
            steps: 1,
            settled: false,
        };
        let mut o = String::new();
        clock(&mut o, Paint(false), &t);
        assert_eq!(
            o,
            "
Clock          ● PTP stopped, holding over
  error        interface wlan0 is down
  interface    wlan0
  frequency    +12.31 ppm
  steps        1
"
        );
    }

    /// The keys of the status rows are texts, and count for its width.
    #[test]
    fn status_keys_are_texts() {
        for key in KEYS {
            assert!(i18n::CATALOG.has(key), "{key}");
        }
        let source = include_str!("main.rs");
        for call in ["row(o, p, \"", "row(&mut o, p, \""] {
            for rest in source.split(call).skip(1) {
                let key = &rest[..rest.find('"').unwrap()];
                assert!(KEYS.contains(&key), "{key}");
            }
        }
    }
}
