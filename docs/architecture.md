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
  consumer routers. Streams therefore default to RTP unicast, because
  multicast floods such networks and Wi-Fi.
- **Decentralized.** Every device is a node with sources and sinks. Nodes
  find each other and connect directly; a controller is optional.
- **Open.** Orpheus is free software (GPL-3.0-or-later) and builds on open
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
| `orpheus-engine` | The media engine: ports between realtime audio and the network, senders, the receiver, ASRC, the `AudioBackend` trait. |
| `orpheus-pipewire` | The PipeWire implementation of `AudioBackend`. |
| `orpheusd` | The node daemon: config, home directory, HTTP API. |
| `orpheusctl` | Command-line client for the HTTP API of any node. |
| `orpheus-discovery` | Announces the node over mDNS and keeps the list of other nodes. |
| `orpheus-nmos` | Placeholder for the NMOS APIs. |

## Audio path

A route from node A to node B:

```text
Node A                                          Node B
app ─▶ sink "B"                                 receiver (UDP 5004)
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
resamples it to the sound card's clock and hands it to a playback stream
through a port.

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
| main | tokio runtime: HTTP API, signals |
| PipeWire | PipeWire main loop; owns all PipeWire objects, which are not `Send`. The engine talks to it through a channel. |
| PipeWire data (RT) | Process callbacks: copy between PipeWire buffers and ports |
| `orpheus-ptp` | PTP: sockets, timers, statime |
| TX, one per sender | Pacing, ASRC, RTP send |
| RX | RTP receive, jitter buffer, ASRC |

The TX, RX and PTP threads run at normal priority for now.

## Network

| Port | Protocol | Use |
|---|---|---|
| 319/udp | PTP event messages | multicast, IPv4 or IPv6 |
| 320/udp | PTP general messages | multicast, IPv4 or IPv6 |
| 5004/udp | RTP | unicast, IPv4 and IPv6 |
| 5353/udp | mDNS | multicast, IPv4 and IPv6 |
| 7480/tcp | HTTP API | IPv4 and IPv6 |

## Stream format

L24 (24-bit big-endian PCM), 48 kHz, 2 channels, 1 ms per packet, RTP payload
type 97. This is the AES67 baseline. It is fixed for now; with SDP it will be
negotiated per stream.

## Identity

A node's ID is 40 random bits, written as 8 characters of Crockford Base32
(`0-9`, `A-Z` without `I`, `L`, `O`, `U`). Parsing is lenient: lowercase is
accepted, `I` and `L` read as `1`, `O` reads as `0`, and hyphens are ignored.
The PTP clock identity is derived from the interface's MAC address instead.
