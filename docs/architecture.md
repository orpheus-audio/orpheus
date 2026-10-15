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
| `orpheus-core` | Platform-independent types shared by all nodes, including the planned ESP32 firmware: device ID, audio format, RTP packets, jitter buffer, media-clock arithmetic. |
| `orpheus-clock` | The `Clock` trait, the system clock on the TAI timescale, and PTP on top of statime. |
| `orpheus-engine` | The media engine: ports between realtime audio and the network, senders, receivers with their gains and level meters, ASRC, the `AudioBackend` trait. |
| `orpheus-pipewire` | The PipeWire implementation of `AudioBackend`. |
| `orpheusd` | The node daemon: config, home directory, HTTP API, NMOS APIs, connecting to other nodes. |
| `orpheusctl` | Command-line client for the HTTP API of any node. |
| `orpheus-discovery` | Announces the node over mDNS and keeps the list of other nodes. |
| `orpheus-nmos` | NMOS IS-04 resources, IDs and versions; IS-05 parameters; writing and reading SDP. |

## Audio path

A route from node A to node B:

```text
Node A                                          Node B
app ─▶ sink "B"                                 receiver k (UDP 5004 + 2k)
        │ PipeWire RT callback                    │ RX thread
        ▼                                         ▼
      port (lock-free ring)                     jitter buffer
        │ TX thread, every 1 ms                   │ on the media-clock grid,
        │ ASRC: sound card → media clock          │ at timestamp + link offset
        ▼                                         │ ASRC: media clock → sound card
      RTP L24/48k/2ch ───── unicast UDP ─────▶    ▼
                                                port (lock-free ring)
                                                  │ PipeWire RT callback
                                                  ▼
                                                playback stream ─▶ output
```

**Sender.** Each sender is a virtual PipeWire sink. Its realtime callback
copies whatever is played into it into a port. A TX thread takes 1 ms of
audio (48 frames) at a time, on the media-clock grid, resamples it to the
media clock, and sends it as one RTP packet. Each packet carries the
media-clock time of its first frame.

**Receiver.** An RX thread stores incoming packets in a jitter buffer indexed
by RTP timestamp. Out-of-order, duplicate and late packets are handled there;
missing audio becomes silence. On the media-clock grid, the thread takes out
the audio whose timestamp is the current time minus the link offset,
passes it through a gain, which fades over 30 ms to the volume the
[policy](#receiver-pool-and-policy) sets, and a level meter, resamples it
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
| main | tokio runtime: HTTP and NMOS APIs, the NMOS refresh, the peer watch, the policy, the health check, scheduled activations, signals; blocking work (engine, peers' APIs) on tokio's blocking pool |
| PipeWire | PipeWire main loop; owns all PipeWire objects, which are not `Send`, and follows the default output and its type and can set the default output, as the desktop does. The engine talks to it through a channel. |
| PipeWire data (RT) | Process callbacks: copy between PipeWire buffers and ports |
| `orpheus-ptp` | PTP: sockets, timers, statime |
| TX, one per sender | Pacing, ASRC, RTP send |
| RX, one per running receiver | RTP receive, jitter buffer, gain and level meter, ASRC |
| mDNS | mdns-sd's daemon, plus `mdns-monitor` (renames on name conflicts) and `mdns-browse` (the node list) |

The TX, RX and PTP threads run at normal priority for now.

## Network

| Port | Protocol | Use |
|---|---|---|
| 319/udp | PTP event messages | multicast, IPv4 or IPv6 |
| 320/udp | PTP general messages | multicast, IPv4 or IPv6 |
| 5004–5018/udp | RTP: receiver *k* on 5004 + 2*k*, up to `[receiver] max` (by default; connections can pick other ports) | unicast; multicast only for interop, IPv4 and IPv6 |
| 5353/udp | mDNS | multicast, IPv4 and IPv6 |
| 7480/tcp | HTTP API | IPv4 and IPv6 |

## Stream format

L24 (24-bit big-endian PCM), 48 kHz, 2 channels, 1 ms per packet, RTP payload
type 97. This is the AES67 baseline. It is fixed for now. Each sender
describes its stream in an SDP file; RTP timestamps count media-clock frames
since the PTP epoch, so the SDP says `mediaclk:direct=0`.

## NMOS

A node is one NMOS device. Its receivers and senders are NMOS resources:
each sender is a source, a flow and a sender. `orpheusd` rebuilds the
resources from the engine once a second and whenever the API is asked; a
resource gets a new version when its content changes. The node then
announces a new `ver_*` counter in its mDNS TXT record, which tells NMOS
controllers and other nodes to fetch the resources again.

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
"always"`, where a route without a receiver claims one at once. The peer
watch ignores what a peer said before the last claim or release, and lets
go of a receiver still connected to an idle route.

A route whose peer has been unreachable for a minute, and isn't pinned,
loses its sink (`offline`): the engine drops the sender and keeps its
configuration, route and sink volume, and starts it again when the peer
answers, so that applications play elsewhere meanwhile and find the same
sink afterwards.

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
  PATCHed again. If the route to the peer now leaves from another source
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
output, else `default_output`, else the system default, or `Held` if that
is a sink to another node. Moves go through the metadata (`-1` for the
default, which also overrides a target in the stream's properties);
holding connects the stream again without autoconnect, so WirePlumber
leaves it unlinked. A new node's volume is set once it reports its
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
(`x-orpheus-link`, that of its default route, checked every 5 s).

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

## Identity

A node's ID is 40 random bits, written as 8 characters of Crockford Base32
(`0-9`, `A-Z` without `I`, `L`, `O`, `U`). Parsing is lenient: lowercase is
accepted, `I` and `L` read as `1`, `O` reads as `0`, and hyphens are ignored.
The PTP clock identity is derived from the interface's MAC address instead.
