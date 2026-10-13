# Roadmap

## Milestones of the Linux node

| | Milestone | Status |
|---|---|---|
| M0 | Skeleton: crates, config, device ID, systemd unit, `orpheusctl status` | done |
| M1 | Local audio path through the engine (PipeWire sink → ports → playback) | done |
| M2 | RTP unicast between two nodes on the system clock; jitter buffer and loss handling | done |
| M3 | PTP, TAI time base, ASRC in both directions | done, except interop tests |
| — | IPv6 for PTP, RTP and the API | done |
| — | Tuning the PTP servo | done, except on real hardware |
| M4 | Discovery (mDNS) and NMOS: IS-04 node API, IS-05 connections with SDP, connecting through IS-05 from `orpheusctl`, output types | done, except interop tests |
| M5 | Receiver pool and policy: one receiver per connection, policies (mix, first sender wins, last sender wins) and ducking by priority; following peers (address changes, reclaiming a receiver); pinned peers and aliases; per-sender volumes | in progress |
| M6 | Configurability: clock settings (settle, slew, failover) over the API, WebSocket state stream, IS-08 and BCP-004 | planned |

After M6 the API should be stable enough for the web UI.

Later, beyond the Linux node: ESP32 receivers, a Wi-Fi profile with
timestamped buffering and adaptive latency, the Svelte web UI (also packaged
with Tauri), Windows and macOS nodes, and optional AirPlay, Spotify Connect
and Bluetooth inputs.

## Known limitations

**Audio**

- The stream format is fixed: L24, 48 kHz, stereo, 1 ms packets, payload type
  97. A receiver refuses SDP files for other formats; without an SDP file it
  accepts any payload type.
- One receiver per node, so a node plays one connection at a time; a second
  `connect` to it fails unless forced. M5 gives each connection its own
  receiver (and port), always keeping one free, and mixes them; see
  [below](#receiver-pool-and-policies).
- Only `POST /x-orpheus/v1/senders` creates senders; IS-05 can redirect,
  pause and resume them, not create them.
- IPv6 multicast groups are joined on the system's default interface, so a
  node without an IPv6 default route can't join them.
- A connection made on a receiver through IS-05 doesn't reach the sender,
  and the other way round; only `orpheusctl connect` (and `disconnect`) set
  up both ends.
- A receiver stays connected to a sender that vanished without
  disconnecting (its node died or lost its state). The next `connect` needs
  `--force`.
- Senders find their peer in discovery by the address they send to; a
  peer that changes its address is not followed, and the sender shows it as
  unreachable.
- A sender whose peer's receiver was taken over stays paused until the
  receiver is connected to it again; it doesn't ask for it back.
- Two nodes connecting to the same peer at the same moment can both pick
  its receiver; the later one wins.
- `connect` accepts a peer with an NMOS API but no `[receiver]` of the right
  format only with `--send-only`.
- The sender sizes its buffer from the configured block (`[audio]
  block_frames` or `cycle_ms`, or assumes 1024 frames) instead of reading
  the block the audio system really runs, and the status doesn't show it.
- The receiver path has two buffers in a row (its port, then PipeWire's
  cycle). Playing out directly in the realtime callback would save a cycle.
- The receiver's playback stream runs even when nothing is sent to it, which
  keeps the PipeWire graph awake.
- The latency a sender reports to PipeWire covers only its own buffer and the
  resampler, not the link offset and the remote playout.
- The resampler always runs, even at ratio 1. Its CPU cost has not been
  measured.
- The network threads run at normal priority, not realtime.

**Clock**

- Software timestamps only. Hardware timestamping is future work, see below.
- The servo was tuned between network namespaces on one machine, with
  artificial jitter and clock drift. Real crystals, NICs and switches may
  need different settings; see [below](#servo-tuning-on-real-hardware).
- The settle criterion is a heuristic: the filter's own offset estimate below
  100 µs for three announce intervals. The filter cannot see its own lag, so
  under fast drift a node can report settled while it is up to a few hundred
  µs behind.
- One interface and one transport per node, end-to-end delay only. The AES67
  message rates are fixed. `[ptp]` can only be set in the config file.
- The PTP clock sits on the system clock, so an NTP step disturbs it.
- Jitter above the step threshold (1 ms), as on Wi-Fi, makes the clock
  step over and over, and a sudden change of the path delay makes it step
  for a few seconds; see [clock](clock.md#ptp). The node warns
  (`ptp_steps`). A larger `step_threshold_us` helps; the planned Wi-Fi
  profile should set it, or the servo should step only on offsets that
  persist.
- PTP needs `CAP_NET_BIND_SERVICE`, which has to be set again after every
  build.

**Control**

- The HTTP API has no authentication.
- NMOS: peer-to-peer discovery only; nodes don't register with an NMOS
  registry. Interfaces don't name the switch port they are attached to
  (`attached_network_device`), and receivers have no BCP-004 capabilities
  yet.
- Network warnings see Wi-Fi only on the two ends' own links; a wireless
  hop in between shows only as jitter. Along a chain of nodes (A to B, B on
  to C), C doesn't learn about Wi-Fi between A and B. Wi-Fi detection is
  Linux-specific (`/sys/class/net`).
- Output types are detected from PipeWire's form factor and profile name
  only; the active port of a sound card (headphone jack or speakers) is not
  looked at. Outputs are matched by their PipeWire name, so a type set for a
  USB device is lost if its name changes.

## Future work

### Hardware timestamping

With hardware timestamps the network card records when a PTP packet passes
the wire, which removes the operating system's scheduling jitter. The error
drops from tens of microseconds to well below one.

Plan:

- A setting `[ptp] timestamping = "auto" | "software" | "hardware"`; `auto`
  uses hardware timestamps when the card supports them and falls back to
  software.
- Open the PTP sockets in hardware mode (`HardwarePTPAll`) and bind them to
  the card's PTP hardware clock (PHC, `/dev/ptpN`). timestamped-socket
  already supports both.
- Preferred: let statime steer the PHC, as statime-linux does, and read media
  time from it (about 1 to 2 µs per read). Alternative: keep the virtual
  clock on the system clock and measure the PHC-to-system offset as phc2sys
  does; that is more code and adds about 1 µs of error.
- It needs a card with a PHC: Intel i210, i225 and i226, many onboard Intel
  NICs, the Raspberry Pi CM4. `ethtool -T <interface>` shows whether a card
  has one. Common USB Ethernet adapters don't.

### Servo tuning on real hardware

The servo settings come from runs on one machine (see
[clock](clock.md#ptp)). Things to check once two machines are available:

- **Settle time and jitter** with real software timestamps (interrupt
  coalescing, CPU power saving), which are heavier-tailed than the simulated
  jitter.
- **Drift.** statime starts by assuming a very stable crystal
  (`initial_wander` 1e-16) and adapts slowly. A clock whose frequency changes
  by 0.1 ppm/s (a fast warm-up) then lags by about 80 µs, at 0.5 ppm/s by
  about 500 µs. A higher `initial_wander` follows drift better but triples
  the error under heavy-tailed jitter. Both are harmless for audio (a 10 ms
  link offset), so the default stays.
- **Measuring sync.** Two machines share no clock, so the true error can't be
  read off as on one machine. The honest test is acoustic: loop one node's
  line out into the other's line in and compare clicks played by both.
- The step bug is worth reporting to statime upstream, as is its initial
  measurement noise of `steer_time`² seconds².

### Robustness outside the development setup

So far everything was tested on one machine: two network namespaces linked by
veth, sharing one kernel clock, with artificial clock skew, about 2 µs of
network delay, and no jitter or loss. Expected problems on real networks:

- **Wi-Fi.** Access points send multicast at low rates and, with power
  saving, hold it back until the next DTIM beacon (100 to 300 ms). PTP may
  never settle, and sync between rooms suffers. The planned Wi-Fi profile
  addresses this.
- **IGMP/MLD snooping without a querier.** Managed switches, and some routers
  and access points, silently stop forwarding PTP multicast after a few
  minutes. Plan: detect it (no announce from the grandmaster for a while) and
  warn; it is described in [troubleshooting](troubleshooting.md).
- **Host firewalls.** firewalld and ufw block UDP 319, 320 and 5004 and TCP
  7480 by default. Plan: detect it, for example when a peer sends but nothing
  arrives.
- **VPNs.** The default route can lead through a VPN (for example a
  full-tunnel WireGuard interface). It has no MAC address, so PTP fails and
  the node falls back to the system clock. Plan: prefer interfaces with a
  MAC, skip tunnels, and warn clearly.
- **NTP steps** of the system clock disturb the PTP clock; slewing is fine.
- **Mixed IPv4 and IPv6.** Nodes on different PTP transports don't see each
  other, and AES67 and Dante devices only speak IPv4.
- **Untested:** more than two nodes, real grandmasters, interoperability with
  Dante, AES67 devices and pipewire-aes67, real crystal drift between two
  machines, network jitter and loss on real hardware, and runs of several
  hours.
- **Packaging.** `setcap` has to be redone after every build. Packages can set
  the capability on install; a system service could use systemd's
  `AmbientCapabilities`.

### Control plane and configuration

- **Split between config and control plane.** Routing lives in the API and
  is saved in the state directory (`routes.json`); `config.toml` holds
  defaults. Still open is how other settings changed through the API (link
  offsets, clock settings in M6) relate to the file, for example the file as
  defaults that the API overrides, with the API showing where each value
  comes from.
- **Output types** (M4). Each output of a device announces a type, and other
  nodes show its sink with a matching icon and PipeWire `device.form-factor`.
  Outputs are typed separately, so an AV receiver's zones can differ. The
  owning device announces the type (detected from its own output where
  possible, otherwise set over the API); like names, every other node can
  override it locally. Announced as the NMOS IS-04 tag
  `urn:x-orpheus:form-factor`. Done in M4, see [usage](usage.md#output-types).

  | Type | form-factor | Icon |
  |---|---|---|
  | `speaker` | speaker | `orpheus-speaker`* |
  | `headphones` | headphone | `audio-headphones` |
  | `headset` | headset | `audio-headset` |
  | `tv` | tv | `video-display` |
  | `hifi` | hifi | `orpheus-hifi`* |
  | `computer` | computer | `computer` |
  | `portable` | portable | `multimedia-player` |
  | `phone` | handset | `phone` |
  | `generic` | – | `orpheus-network-speaker`* |

  \* Shipped with Orpheus: `speaker-front`, `sound` and `wireless-speaker`
  from GNOME's icon development kit (CC0), renamed with an `orpheus-` prefix
  so that no theme replaces them with an unrelated icon of the same name
  (Papirus, for example, has a `sound` icon for sound files). They live in the
  `hicolor` fallback theme and look like Adwaita in every theme. The table is
  configurable in `[icons]`; the type itself and the form-factor are not.

  `--send-only` sinks are `generic` unless set. Later, as an advanced
  option, a raw icon name per sink.
- **Source types** (todo): the same for inputs, such as microphone, turntable
  or TV audio.

### Receiver pool and policies

Planned for M5. The receiving node decides what it plays: a sender always
connects to a free receiver, and the node's policy only sets each stream's
volume. Connections are never refused or taken over because of the policy.

- **One receiver per connection.** A node advertises one NMOS receiver more
  than it has connections, so there is always a free one, up to `[receiver]
  max` (8). Receiver *k* listens on port 5004 + 2*k* (firewalls should allow
  5004–5018) and has its own playback stream, named after its sender and
  running only while it is connected (with `accept = "any"`, the free
  receiver runs too and plays unconnected streams). PipeWire sums the
  streams, so mixing costs nothing extra. WirePlumber remembers each stream's
  volume and output per sending node.
- **Policies**, set per node with `[receiver] policy`: `last` (the default)
  plays whoever started playing most recently and fades the others out; when
  it stops, the one before comes back. `first` keeps playing whoever started
  first until it stops. `mix` plays everything. "Playing" means sound above
  -60 dBFS within the last 5 s, not merely a connection, so a paused phone
  doesn't block the speaker. A muted sender shows `muted by policy`.
- **Ducking.** A connection can have priority `high` (`connect --priority
  high`, for a doorbell or announcements). While a high-priority stream
  plays, all others are lowered by `[receiver] duck_db`; the policy applies
  among the normal ones.
- **Following peers.** A sender follows its peer by node ID when its
  address changes, takes a free receiver again when its own was taken over,
  and checks after connecting that it won a race for the receiver. A
  receiver whose sender no longer exists is freed.
- **Pins and aliases.** A node can pin peers, which stay listed while
  offline, and give them local names, which name their sinks.

Sample-rate changes come with the AES67 extended mode (44.1, 96 kHz), as the
native format is fixed.

### Graphs

Graphs of the PTP offset, latency and ASRC over time are left to the web UI;
`orpheusctl status --watch` shows the current values only.

### Platforms and audio backends

- **ALSA backend** for headless Linux nodes without a sound server, such as a
  Raspberry Pi behind an amplifier. It needs its own realtime thread with
  period handling, xrun recovery, device selection and format conversion, all
  of which PipeWire does today. It can use the sound card directly, which
  lowers latency.
- PulseAudio-only systems and JACK are not planned; PipeWire covers both.

### Logging

Under systemd, log lines carry their level as the journal priority. The
fields (`error=…`, `addr=…`) are part of the message text, not separate
journal fields. Native journal fields (for example with `tracing-journald`)
would allow queries such as `journalctl F_ERROR=…`, but that crate puts the
fields only into separate journal fields, so the reason would disappear from
plain `journalctl` output. Revisit if the UI or tooling needs to query the
journal.

### Dependencies

`rand` stays at 0.8 because statime 0.4 expects a random number generator
implementing rand 0.8's `Rng`. Upgrade both together.
