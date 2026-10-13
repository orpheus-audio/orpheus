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
| M4 | Discovery (mDNS) and NMOS: IS-04 node API, IS-05 connections with SDP, connecting through IS-05 from `orpheusctl` | in progress: mDNS, IS-04, SDP, IS-05 done |
| M5 | Memory and policy: pinned peers, aliases, route restore on reconnect, per-sender gains; mixing policies and ducking; sample-rate changes | planned |
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
- One receiver per node, following one stream at a time. Mixing comes in M5.
- Only `POST /x-orpheus/v1/senders` creates senders; IS-05 can redirect,
  pause and resume them, not create them.
- Multicast (interop only) has no guard rails yet: nothing warns when a
  multicast stream runs over Wi-Fi. `orpheusctl status` and the log should.
- IPv6 multicast groups are joined on the system's default interface, so a
  node without an IPv6 default route can't join them.
- A connection made on a receiver doesn't reach the sender, and the other
  way round: a controller patches both, or `orpheusctl connect` patches the
  sender only.
- The sender sizes its buffer from `[audio] quantum` (or assumes 1024 frames)
  instead of reading the real quantum.
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
- PTP needs `CAP_NET_BIND_SERVICE`, which has to be set again after every
  build.

**Control**

- The HTTP API has no authentication.
- Nodes are discovered, but peers are still addressed by host name or IP.
- NMOS: peer-to-peer discovery only; nodes don't register with an NMOS
  registry. Interfaces don't name the switch port they are attached to
  (`attached_network_device`), and receivers have no BCP-004 capabilities
  yet.
- The output type of a node's own output is not detected yet; its receiver
  announces `generic`.

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
- **Connecting by name** (M4): `orpheusctl connect Kitchen`, resolved through
  discovery by the node's announced name or ID instead of a host name.
- **Output types** (M4). Each output of a device announces a type, and other
  nodes show its sink with a matching icon and PipeWire `device.form-factor`.
  Outputs are typed separately, so an AV receiver's zones can differ. The
  owning device announces the type (detected from its own output where
  possible, otherwise set by config or API); like names, every other node can
  override it locally. Announced as an NMOS IS-04 tag such as
  `urn:x-orpheus:form-factor`.

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

  For now, every network sink is `generic`.

  Later (M4/M5), stored in the state directory like aliases: a local type
  override per sink, and as an advanced option a raw icon name per sink.
- **Source types** (todo): the same for inputs, such as microphone, turntable
  or TV audio.

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
