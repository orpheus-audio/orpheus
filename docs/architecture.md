# Architecture

Orpheus is an audio network for homes: ordinary Ethernet, cheap unmanaged
switches, consumer routers and Wi-Fi, not a studio network with managed
switches and a dedicated grandmaster.

This document describes the current implementation, the Linux node built on
PipeWire. Nodes for Windows, macOS, ESP32 and other devices will follow. They
speak the same protocols on the network but are built differently inside, with
their own audio backends, threads and buffers. What they share is described
under [Design goals](#design-goals), [Network](#network), [Stream
format](#stream-format) and [Identity](#identity); the rest is specific to
Linux.

## Design goals

- **Cheap home networks.** Everything must work on unmanaged switches and
  consumer routers. Orpheus' own streams are therefore always RTP unicast,
  because multicast floods such networks and Wi-Fi. Multicast exists only
  for interoperability with AES67 and Dante devices and NMOS controllers,
  and only when one of them asks for it.
- **Decentralized.** Every device is a node with sources and sinks. Nodes
  find each other and connect directly; a controller is optional.
- **Open.** Orpheus is free software (GPL-3.0-only) and builds on open
  standards instead of proprietary protocols: AES67 for audio, PTP (IEEE 1588)
  for time, NMOS for discovery and control, mDNS for finding nodes.
- **Interoperable.** Because the protocols are standard, Orpheus nodes can
  work with AES67 and Dante devices, and other software can control them
  through NMOS.
- **Extensible.** New platforms only need to implement the same protocols, and
  new features build on the standards' extension points: Orpheus-specific API
  lives under `/x-orpheus`, next to the NMOS APIs, and optional inputs such as
  AirPlay, Spotify Connect or Bluetooth are compile-time features.
- **Node-agnostic control.** The UI treats the local node like any remote
  one, and a node needs no UI.

## Crates (Linux node)

| Crate | Role |
|---|---|
| `orpheus-core` | Platform-independent types shared by all nodes, including the planned ESP32 firmware: device ID, audio format, channel layouts, RTP packets, jitter buffer, media-clock arithmetic. |
| `orpheus-clock` | The `Clock` trait, the system clock on the TAI timescale, and PTP on top of statime. |
| `orpheus-engine` | The media engine: ports between realtime audio and the network, senders, receivers with their level meters and gains, ASRC, the `AudioBackend` trait. |
| `orpheus-pipewire` | The PipeWire implementation of `AudioBackend`. |
| `orpheusd` | The node daemon: config, home directory, HTTP API, NMOS APIs, connecting to other nodes. |
| `orpheusctl` | Command-line client for the HTTP API of any node. |
| `orpheus-discovery` | Announces the node over mDNS and keeps the list of other nodes. |
| `orpheus-nmos` | NMOS IS-04 resources, IDs and versions; IS-05 parameters; writing and reading SDP. |
| `orpheus-i18n` | Texts in the user's language from compiled-in Fluent files, for `orpheusd` and `orpheusctl`. |
| `orpheus-aes70` | The device side of AES70: OCP.1 messages and data types, the managers, a tree of blocks, gains, mutes and level meters, and subscriptions. `no_std` with `alloc`, for the ESP32 firmware too. |

## Audio path

A route from node A to node B:

```text
Node A                                          Node B
app ─▶ sink "B"                                 receiver k (UDP 5004 + 2k)
        │ PipeWire RT callback                    │ RX thread
        ▼                                         ▼
      port (lock-free ring)                     jitter buffer
        │ TX thread, every packet (1 ms)          │ on the media-clock grid,
        │ ASRC: sound card → media clock          │ at timestamp + link offset
        ▼                                         │ ASRC: media clock → sound card
      RTP L24/48k ───────── unicast UDP ─────▶    ▼
                                                port (lock-free ring)
                                                  │ PipeWire RT callback
                                                  ▼
                                                playback stream ─▶ output
```

**Sender.** Each sender is a virtual PipeWire sink. Its realtime callback
copies whatever is played into it into a port. A TX thread takes one
packet's worth of audio at a time (1 ms, 48 frames; 125 µs, 6 frames, for
more than 8 channels), on the media-clock grid, resamples it to the media
clock, and sends it as one RTP packet. Each packet carries the media-clock
time of its first frame.

**Receiver.** An RX thread stores incoming packets in a jitter buffer indexed
by RTP timestamp. Out-of-order, duplicate and late packets are handled there;
missing audio becomes silence. On the media-clock grid, the thread takes out
the audio whose timestamp is the current time minus the link offset,
passes it through a level meter and a gain, which fades over 30 ms to the
volume the [policy](#receiver-pool-and-policy) sets, resamples it
to the sound card's clock and hands it to the receiver's own playback
stream through a port. PipeWire mixes the playback streams.

**Ports.** A port is a single-producer, single-consumer ring of `f32`
samples. Both ends are realtime-safe: no allocation, locks or syscalls. The
reading side keeps a fixed number of frames in the ring, which sets its
latency. Before it plays it waits until that much is buffered ("priming").
After an underrun it primes again, and it drops audio when the ring grows far
beyond its target ("resync").

**Grid.** The network threads wake up at media-clock times. If a thread
stalls, or the clock jumps, by more than 100 ms it restarts at the current
time instead of catching up ("realign").

**Latency.** End to end, audio is delayed by:

- the sender's port: one PipeWire cycle plus 2 ms,
- the link offset (default 10 ms), which includes the 1 ms packet,
- the receiver's port: 4 ms plus one PipeWire cycle, because the RX thread
  writes 1 ms at a time while PipeWire reads a whole cycle,
- PipeWire's own buffering at both ends,
- the resamplers, about 32 frames (0.7 ms) each.

## Threads

| Thread | Work |
|---|---|
| main | tokio runtime: HTTP, NMOS and AES70 APIs, the NMOS refresh, the peer watch, the policy, the health check, scheduled activations, signals; blocking work (engine, peers' APIs) on tokio's blocking pool |
| PipeWire | PipeWire main loop; owns all PipeWire objects, which are not `Send`, and follows the default output and its type and can set the default output, as the desktop does. The engine talks to it through a channel. |
| PipeWire data (RT) | Process callbacks: copy between PipeWire buffers and ports; play the wires at their gains; measure the wires and the outputs they play on |
| `orpheus-ptp` | PTP: sockets, timers, statime; restarts the port when its interface changes or the settings do, and retries while it can't start |
| TX, one per sender | Pacing, ASRC, RTP send |
| RX, one per running receiver | RTP receive, jitter buffer, level meter and gain, ASRC |
| mDNS | mdns-sd's daemon, plus `mdns-monitor` (renames on name conflicts) and `mdns-browse` (the node list) |

The TX, RX and PTP threads run at normal priority for now.

## Network

| Port | Protocol | Use |
|---|---|---|
| 319/udp | PTP event messages | multicast, IPv4 or IPv6 |
| 320/udp | PTP general messages | multicast, IPv4 or IPv6 |
| 5004–5018/udp | RTP: receiver *k* on 5004 + 2*k*, up to `[receiver] max` (by default; connections can pick other ports) | unicast; multicast only for interop, IPv4 and IPv6 |
| 5353/udp | mDNS | multicast, IPv4 and IPv6 |
| 7480/tcp | HTTP API; AES70 over WebSocket | IPv4 and IPv6 |
| 7481/tcp | AES70 (OCP.1) | IPv4 and IPv6 |

## Stream format

L24 (24-bit big-endian PCM), 48 kHz, RTP payload type 97, with 1 to 64
channels: as many as the route's layout has, stereo unless set otherwise.
The packet time follows from the channel count, as the levels of SMPTE ST
2110-30 have it: 1 ms for up to 8 channels (level A, the AES67 baseline),
125 µs for more (level C), so that a packet stays within an Ethernet frame
(at most 1152 bytes of audio). Each sender describes its stream in an SDP
file; RTP timestamps count media-clock frames since the PTP epoch, so the
SDP says `mediaclk:direct=0`.

The receiver doesn't care about the packet time: its jitter buffer is
indexed by timestamp and read every millisecond, whatever size the packets
are. It plays as many channels as the SDP file says, else two.

Without optimization, the resampler can't keep up with many channels at
short packets, so debug builds optimize it (`[profile.dev.package]` in
`Cargo.toml`).

## Channel layouts

A `Layout` (`orpheus-core`) says what each channel of a stream carries: an
NMOS channel symbol and an optional label. It is Orpheus' own model; each
interface translates at its edge and nothing Linux-specific goes on the
wire:

- **SDP:** written and read as the ST 2110-30 `channel-order`. It knows
  fewer layouts than the symbols; runs of channels without a group go as
  undefined (`U02`), labels don't go at all.
- **IS-04:** a source's `channels`, symbol and label each.
- **PipeWire:** channel positions of the sink and the playback stream;
  symbols without a position become numbered AUX channels.

A receiver's layout is resolved when its stream starts, in this order: the
layout set for its [source](#receiver-pool-and-policy) (`sources.json`),
the stream's SDP file, then a default by channel count. A change of the
layout restarts the stream, as a change of the link offset does.

A sender's layout is its route's: `ST` unless set, kept with the route in
`routes.json`. Changing it starts a new sink of the same name, with the new
channels, and removes the old one once PipeWire shows the new one; the
session manager then moves what played into the old sink to the new one
by name, so nothing goes silent or plays elsewhere meanwhile. The sender
holds its packets back while the peer's receiver gets the new SDP file
and restarts with it, so that it never reads packets in the wrong layout.

## NMOS

A node is one NMOS device. Its receivers and senders are NMOS resources:
each sender is a source, a flow and a sender. `orpheusd` rebuilds the
resources from the engine once a second and whenever the API is asked; a
resource gets a new version when its content changes. The node then
announces a new `ver_*` counter in its mDNS TXT record, which tells NMOS
controllers and other nodes to fetch the resources again.

Receivers announce what they play in their capabilities (BCP-004-01): one
constraint set with media type, sample rate, sample depth and a channel
count from 1 to 64, at any packet time. Before `connect` takes a peer's
receiver, it checks the receiver's constraint sets against the route's
stream; constraints on other parameters are
ignored, receivers without constraint sets are judged by `media_types`.

There is no IS-08 Channel Mapping API: [wires](usage.md#wires) route
channels, across all sources, inputs and outputs. IS-08 maps each output
channel to one input channel and so can't show channels summed where
wires meet; a view that hid them would mislead a controller.

Each sender's `manifest_href` points to its SDP file. Its session version
follows the sender's and the node's resource versions, so it changes when the
destination or the PTP grandmaster does.

Resource IDs are UUIDv5 from the node ID and the resource's path
(`node`, `device`, `receiver/0`, `sender/<route ID>`, `source/<route ID>`,
…). A sender is named by its route: a random ID like the node ID (40 bits,
8 characters of Crockford Base32, unique among the node's routes), made by
the first `connect` to a peer
with a priority, kept in `routes.json` while the route runs and after it
is disconnected (within the `[routes]` limits), and reused by the next
`connect` to the same peer (by API or node ID) with the same priority. The
route ID names its sink (`orpheus.send.<route ID>`) and appears in logs,
the API and orpheusctl; the sender's NMOS source ID, derived from it, is
`a=x-orpheus-source` in its SDP file, globally unique. A
receiver keys what it remembers of a stream on `orpheus.source.<ID>`;
streams from other senders fall back to the SDP origin's user name,
session ID and address, then to the source address (see below).

## Connections

Routing goes through IS-05: every sender and receiver has staged and
active parameters. Activating them changes the engine at runtime without
restarting streams:

- A sender sends to another address or port, from another source address
  or port (the socket is reopened), or pauses. Multicast destinations need
  nothing special when sending.
- A receiver filters what it plays by source address and payload type (from
  the sender's SDP), joins and leaves multicast groups, and moves to another
  UDP port. A new filter drops the stream played so far, so the next one
  starts at once.

A receiver without a source address or group plays nothing, unless
`[receiver] accept = "any"`. Its playback stream takes the name in the
connected sender's SDP file (`s=`, which Orpheus fills with the node's
name).

`orpheusctl connect` (`POST /x-orpheus/v1/routes`) is a client of the
peer's APIs (`peer.rs`, blocking HTTP without holding any lock): it reads
the peer's IS-04 receivers, picks one without a sender, reads its port
from IS-05, starts the sender, and PATCHes the receiver with the sender's
SDP file. Then it reads the receiver's IS-04 subscription back: if another
node's PATCH came in between, it tries once more with another free
receiver, and else stops the sender with `409`. If the PATCH fails the
sender is removed again. The peer's receiver is kept with the sender, by
its API, its ID and the peer's node ID, so `disconnect` can release it,
but only if it still names this sender. A second connection to the same
peer (by API or node ID) with the same priority is refused.

An activation gives the IS-04 resource a new version even where its content
stays the same, as NMOS asks. Scheduled activations run on a timer; while
one is pending, the staged parameters are locked (`423`) until it fires or is
cancelled.

Each sender meters what arrives from its sink on the TX thread: whether
audio arrives at all (`playing`) and when it last carried sound above −60
dBFS. With `[sender] claim = "on_play"`, a task looks at these ten times a
second (`connection/claim.rs`): a route without a receiver claims one when
its sink starts playing, carries sound or becomes the default output, the
way `connect` does (pick a free receiver, point the sender at it, PATCH it,
read it back), and an idle one lets it go after `release_after_s` of
silence. While a route has no receiver, its sender sends nothing; when it
resumes, it fades in over `fade_in_ms`. A route whose peer has no free
receiver waits (`busy`) and tries every 5 seconds, also with `claim =
"always"`, where a route without a receiver claims one at once. One whose
peer has no receiver that fits (`422`, `incompatible`) tries again when the
peer announces a new receiver change counter (`ver_rcv`), or every minute
if discovery doesn't see the peer. The peer
watch ignores what a peer said before the last claim or release, and lets
go of a receiver still connected to an idle route.

A receiving node says in its receivers' state tag when it plays one's
stream nowhere: `lost` while its output is gone (see below), `parked`
after its last wire was removed; these win over the policy's `muted` and
`ducked`. The tag changes the receiver's version and so the announced
`ver_rcv`, on which the sending node asks again at once. While the peer can't play the stream (parked
there, or unreachable for `lost_after_ms`), `tend_sinks` in
`connection.rs` shows the sink under a label, `TV (unavailable)` (see
[languages](#languages)), which the engine shows instead of the name
without changing it. It hides the sink once
nothing is linked into it: links, not audio, so that a paused player's
sink stays and the player moves nowhere when it resumes. Hiding drops
the sender and keeps its configuration, route, note and sink volume;
the engine starts it again when the peer can play the stream, so that
applications find the same sink. Sinks to pinned peers stay; with
`when_lost = "default"`, a sink is hidden once the peer has been
unreachable for a minute (`offline`), linked or not, as WirePlumber then
moves its applications on.

With `when_lost = "pause"`, a sink becoming unavailable also pauses
players, in `players.rs`, through MPRIS on the session bus (`zbus`). The
backend lists the streams applications play (`app_streams`: running
`Stream/Output/Audio` nodes, the outputs they are linked to, and their
client's `application.process.id`, kept only where `/proc` shows that
process running the program the client names, which a process in another
PID namespace, such as a Flatpak sandbox, fails). A player's process is
the one its bus name belongs to (`GetConnectionUnixProcessID`), matched
to the stream's process or its nearest parent running the same program,
as browsers play from a helper process. A process is paused only if all
its streams go to unavailable sinks; then each of its players that says
`Playing` gets `Pause`. Without a session bus nothing is paused. Each
sink pauses once, when it becomes unavailable; nothing resumes.

`routes.json` in the state directory keeps, for each running route, its
ID, name, last destination, the peer's output type and receiver; the
disconnected routes remembered; and for each port the parameters last
activated as requested, `auto` included. At start the node recreates the senders and
activates these again, so `auto` is resolved afresh (a source port, for
example, need not be free any more).

## Output types

The PipeWire thread follows the audio sinks, the default one (from the
`default` metadata) and the links into them, and keeps a copy for the other
threads. Each of our streams notes its node ID, so the engine can ask which
sink the receiver's stream is linked to. `outputs.rs` adds the user's types
from `outputs.json`; the IS-04 receiver carries the result. A sender writes
its peer's type (or the user's) into its sink's `device.form-factor` and
icon, and PipeWire reports the change like any other sink's, which is how
types pass along chains of nodes.

The thread also follows the audio sources and the ports of all audio
nodes; a node's channels are its ports in order with their positions,
read back into a [layout](#channel-layouts). A created output is a
`support.null-audio-sink` node that orpheusd owns, with
`monitor.channel-volumes`, so its volume applies to what it passes on.
Its monitor ports are linked to the chosen input ports of the device.
Whenever ports or links come or go, a timer on the PipeWire thread
(every 200 ms, only after a change) makes the links that are missing, so
an output whose device was unplugged plays there again once it is back.
The node and its links are destroyed with the handle, and with orpheusd,
since they don't linger.

[Wires](usage.md#wires) are links made the same way, through a node of
their own. `wires.rs` keeps them by their ends: a source (by its ID, not
the receiver playing it) or an input, and an output, with channel pairs
by number, symbol or label, or none for all, and gains for the wire and
each pair. Every second, after the sources are placed, it resolves them
against the running receivers and the inputs and outputs there are into
links by node name and channel index, each with its gain as a factor,
and hands the list to the engine. Unchosen pairs that fold a channel onto
another get -3 dB. A source with wires is placed nowhere
(`Placement::Held`), so the session manager doesn't link it as well.

The PipeWire thread makes a wire node (`wire.rs`) for each pair of nodes
the links join while both are there: a `pw_filter`, through the C API
since pipewire-rs has no binding for it, with an input and an output port
per channel pair. Its process callback runs on the realtime thread in the
same graph cycle as its ends, so it adds no latency, and plays each pair
through a `Ramp` from `orpheus_engine::gain`, which moves to a new gain
in a straight line over 960 frames (20 ms at 48 kHz); the gains are
atomics the PipeWire thread sets. The gain code is plain Rust, for other
platforms' nodes to use too. The same timer links the source's ports to
the node's inputs and its outputs to the output's ports; a changed gain
only sets the atomic, while changed pairs make the node again. A new wire
list is applied at once, not only at the next tick. The node is dropped
after its links, which it would otherwise take along under the reconciler.
PipeWire sums what meets at an input port.

Each output the links play on gets a meter node too: a passive
`pw_filter` with an input per channel, linked to the output's monitor
ports, so it measures the sum, also what else plays there, without keeping
the output running. The wire nodes measure each pair after its gain. The
realtime callbacks add each cycle's samples to a `SharedLevel` from
`orpheus_engine::level` (atomics: the peak, the sum of squares in fixed
point, the frames, the samples at full scale); every 100 ms the PipeWire
thread takes them into `Meter`s, which hold the peak over a second and the
RMS over 300 ms, and publishes the levels for `/levels` and the health
check, which warns while an output reached full scale in the last 10
seconds.

`aes70.rs` shows the wires to AES70 controllers. While any is
connected, it builds an `orpheus_aes70::Tree` from the wires, the outputs
and the levels every 100 ms; `Device::set_tree` compares it with the last
one property by property, and each connection turns the changes into
notifications for what its controller subscribed to. Object numbers come
from `aes70.json`, keyed by what each object stands for (`output/<name>`,
`wire/input:<name>/<output>/gain`, …), so they outlive restarts. A
controller's `SetGain` or `SetState` comes back from the device as a
`Set`, which `aes70.rs` carries out on the wires as the API would, then
builds the tree at once, so that every controller hears of it. OCP.1
arrives over TCP (its own listener) or as binary WebSocket messages on
the API's router; both go through the same connection loop, which also
sends keepalives at the interval the controller asks for and drops a
controller silent for three of them.

`connection.rs` watches the peers' receivers the senders are connected to.
Discovery keeps each node's receiver change counter (`ver_rcv` in its mDNS
TXT record), which the peer's NMOS store bumps when a receiver's resource
changes: its type tag or its subscription. Once a second the watch
compares, for each connected sender, the peer's announcement (online,
name, counter) with the last one seen, and asks the receiver again through
IS-04 when it changed or 30 seconds have passed; peers that discovery
doesn't see (reached by URL, or mDNS blocked) get only the 30-second check.
The answer updates the sink's type and name and the link's state; a
receiver that plays another sender pauses the sender in the engine,
separately from its IS-05 route, so activations don't undo the pause.

The watch also keeps a connection whole when things move:

- **Follow.** If discovery's node with the stored node ID no longer
  serves the stored API, the peer has a new address; the sender is
  retargeted to the receiver's port at the new address and the receiver
  PATCHed again. An address ranked below the current one (IPv6 for an
  IPv4 route) counts only once the peer stops answering at the current
  one: mDNS may announce a node's IPv6 address before its IPv4 one. If the route to the peer now leaves from another source
  address than the receiver was given (`source_ip` `auto`), the receiver
  is PATCHed with the new one.
- **Reclaim.** A link notes when its receiver was taken by another sender
  (`displaced`, not saved). While displaced and disconnected, the sender
  asks the peer for a free receiver on every check and moves there:
  retarget, PATCH, verify. A receiver that plays nothing, without having
  been taken, was released on purpose, and the sender stays paused.
- **Check back.** On the receiving side, each connected receiver whose SDP
  names an Orpheus node (`a=x-orpheus-node`) asks that node every 30
  seconds, at its discovered API or at port 7480 of the source address,
  whether its `/self` is still that node, the sender exists (`404` means
  no) and the sender's subscription names this receiver. If not, the
  receiver is freed and the pool shrinks.

## Receiver pool and policy

The engine runs any number of receivers, each with its own socket, RX
thread and playback stream. `connection.rs` keeps the pool: slot *k* has
NMOS ID `receiver/k` and port `[receiver] port + 2k`; there is always one
free slot more than connected ones, up to `max`. A slot saved for a
connection in `routes.json` is never handed to another, and free slots
beyond the lowest go after 60 seconds, so that a controller that just saw
one can still use it. A receiver runs in the engine only while it is
connected, or while free with `accept = "any"`. Its PipeWire stream has
`node.name` `orpheus.receive.<k>` and the source's key as
`application.id`, for WirePlumber's rules; the description is the alias or
the SDP's `s=`.

`sources.rs` keeps the output and volume per source in `sources.json`
(short `SourceId`s, evicted by `[sources]`), not WirePlumber, whose
`stream-properties` cannot be edited while it runs and would outlive what
Orpheus forgets: the streams set `state.restore-props` and
`state.restore-target` to `false`. The engine's backend takes a
`Placement` (`Default`, `Output(name)`, `Held`) and a volume when it
creates a playback stream, and reports the volume (`channelVolumes` and
`mute` controls) and the output the user chose: a `target.object` or
`target.node` in the default metadata that Orpheus did not write. Every
second `sources.rs` records both and places each stream: the remembered
output, else `default_output`, else the default the user chose
(`default.configured.audio.sink`, which WirePlumber doesn't change when
that output goes; else `default.audio.sink`) by name, or `Held` if that is
a sink to another node. Moves go through the metadata, by the output's
serial (the default's too: Orpheus follows the default itself); holding connects the stream again without autoconnect, so
WirePlumber leaves it unlinked.

The streams carry `node.dont-fallback` and `node.linger`: when their
target goes, WirePlumber neither moves them to the default nor destroys
them, they wait unlinked. A returning output has a new serial, which the
metadata target doesn't match, so `sources.rs` places the stream again:
when an output it noted as lost is back, or when a stream is not linked
to an output that is there for a tick (it went and came back between two
ticks). An output gone for `lost_after_ms` parks the stream (status
`parked`); with `when_lost = "default"` the stream is placed as if it had
no output remembered. A source whose last wire was removed is parked in
`sources.json` (`parked`) and `Held` until played or wired again. A new node's volume is set once it reports its
controls; without one remembered it starts at full volume on every
channel, which WirePlumber's restore would otherwise have set, and
without which mixers find no channels to set. Sinks to other nodes work
the same way for their volume: `connection.rs` notes it every second in
the route's entry in `routes.json`, also for remembered routes, and
passes it to the sink when the route starts again.

`policy.rs` runs every 50 ms. It reads each receiver's last sound from the
engine's meter (peak above -60 dBFS, measured before the gain) and the
connection's priority from the SDP (`a=x-orpheus-priority`), and a pure
function `decide` gives each receiver its state (`silent`, `playing`,
`muted`, `ducked`) and gain. A stream plays until 5 s after its last
sound. High-priority streams are always heard and duck the normal ones by
`duck_db`; among the normal ones, `last`, `first` or `mix` decide. A
silent stream gets the gain it would get on starting, so its first sound
is not cut. States other than `silent` and `playing` go into the IS-04 tag
`urn:x-orpheus:state`, which the senders' peer watch reads.

## Peers

`peers.rs` keeps what this node remembers about others by node ID, in
`peers.json`: an alias, a pin, and for pinned peers how they were last
seen (name, version, host, addresses, port), so that `GET /nodes` can list
them offline after a restart. The peer watch updates the last-seen record.
Aliases are looked up wherever a peer is shown: in `peer::resolve` (before
name, ID, host and address), in the names of sinks not named by the user,
and in the receivers' stream names. Changing an alias renames these at
once. Lock order: connections, engine, peers.

## Health

`health.rs` checks the network paths once a second and keeps a list of
warnings, which the status shows and the log follows. Wi-Fi is seen on
this node's own interfaces (`network.rs`: the route the kernel picks for
a peer, a multicast group or PTP, and whether that interface is wireless)
and, for the other end, in the link each node announces over mDNS
(`x-orpheus-link`, that of its default route). Every 30 s the loop
looks at the network again: the node's own link, whether power saving is
on for the Wi-Fi interfaces it uses (`network.rs` asks nl80211, as `iw`
does), and the nodes PTP also sends to by unicast, which it hands to the
PTP thread.

The receiver measures, per second, the spread of the packets' arrival
margins: the latest minus the earliest relative to their timestamps. That
catches Wi-Fi and congestion anywhere on the way. While PTP is settling,
and for 10 seconds after it steps, the check is skipped and its history
cleared, because the clock's own corrections spread the margins too. A receiver that warns
about it adds the tag `urn:x-orpheus:warnings: ["jitter"]` to its IS-04
resource; the new version bumps `ver_rcv`, the senders' peer watch asks
the receiver again and warns on its side too. The clock warns about a
slow path (mean delay) and about repeated steps, which is how a slow path
shows when the filter can't settle.

## Settings

`settings.rs` holds the settings that can change while the node runs in
three layers: the built-in defaults, `config.toml` (which keys it sets is
remembered, to tell the two apart), and the API's changes, a JSON object
of sections and keys in `settings.json`. A change is a JSON merge patch on
that object; it is merged, checked as a whole against the same rules as
the file, applied, and only then saved, so a refused change leaves
nothing behind. The node never writes `config.toml`.

Applying a change touches only what it changes. New PTP settings restart
the PTP port in its thread, while the clock on top of the system clock
keeps its time and rate (see [clock](clock.md#when-ptp-cant-run)); if
the port can't start with them, the old settings stay. A new
`link_offset_ms` restarts the running receivers; the policy, ducking,
default output, sender and `ui` settings are read where they are used.

## Languages

Everything Orpheus shows to people is in their language: the names in the
audio system (a sink's `TV (unavailable)`, the loopback, the wire and
meter nodes), the [warnings](usage.md#warnings), the errors in settings,
and all of `orpheusctl`, its help included. Logs stay in English, and so
do errors that come from the system or from libraries (a socket's
`Permission denied`, a JSON parser's message), which appear inside the
texts as they are. The API's own error messages are English for now.

`orpheus-i18n` does the work: a `Catalog` holds a program's Fluent files,
compiled in, and gives `Texts` for a language; English lies under every
other language, so a missing translation shows English. A `Text` is a
message ID with its arguments, formatted later in any language, which is
how `orpheusd` shows a warning in the node's language and logs it in
English. The files:

- `orpheusd/locales/<language>/orpheusd.ftl`, in `[ui] language`; with
  `auto`, the default, in the environment's;
- `orpheusctl/locales/<language>/orpheusctl.ftl`, in the environment's
  language (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`), whatever the
  node's. clap can't translate its help, so `orpheusctl` sets each
  command's and argument's text from `cmd-<command>` and
  `arg-<command>-<argument>`. clap's own error messages stay English.

Languages are negotiated against those there are (`de-AT` gets `de`).
Each message is a whole sentence or name, so that a language can order
it as it needs; plurals choose by a `$count`. Tests check that every
language has every message, that the code asks only for messages English
has, and that every command and argument has its help. To add a language,
add its files and their lines in the two `Catalog`s.

## Events

`events.rs` serves the WebSocket `/x-orpheus/v1/events`. While at least
one client is connected, a task builds each topic's state as its GET does
(the same functions) every 250 ms, the dearer ones (status, outputs,
sources) every second, and publishes the JSON of the topics that changed
on a `tokio::sync::watch` channel holding the latest message per topic.
Each client sends what changed since it last sent, so a slow client skips
states instead of queueing them, and a client never gets a topic it
already has. Without clients nothing is built.

## Identity

A node's ID is 40 random bits, written as 8 characters of Crockford Base32
(`0-9`, `A-Z` without `I`, `L`, `O`, `U`). Parsing is lenient: lowercase is
accepted, `I` and `L` read as `1`, `O` reads as `0`, and hyphens are ignored.
The PTP clock identity is derived from the interface's MAC address instead.
