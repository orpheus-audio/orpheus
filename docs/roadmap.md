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
| M5 | Receiver pool and policy: one receiver per connection, policies (mix, first sender wins, last sender wins) and ducking by priority; following peers (address changes, reclaiming a receiver); pinned peers and aliases; per-sender volumes (through WirePlumber) | done, except the two-box test |
| — | Route identity: stable route and source IDs, output and volume remembered per source, claiming the peer's receiver on play, default output | done |
| M6 | Configurability: settings over the API (clock, receivers, senders), PTP that retries and holds over, WebSocket state stream, output selection, BCP-004, IS-08 per receiver (removed again in M7, replaced by wires) | done, except the two-box test |
| M7 | Channels and patching: channel layouts after ST 2110 (symbols and labels on the wire), inputs and outputs with their channels, outputs made of chosen device channels, a patchbay wiring any channel of any receiver or local input to any output channel (summing where wires meet), more than 2 channels per route | in progress: layouts, inputs and outputs, wires, mix display, lost outputs (parking, unavailable sinks, pausing players), English and German names, multichannel routes (up to 64 channels), gain per wire, meters, AES70 done |

After M7 the API should be stable enough for the web UI.

Later, beyond the Linux node: ESP32 receivers, a Wi-Fi profile with
timestamped buffering and adaptive latency, the Svelte web UI (also packaged
with Tauri), Windows and macOS nodes, and optional AirPlay, Spotify Connect
and Bluetooth inputs.

## Known limitations

**Audio**

- The stream format is L24 at 48 kHz, payload type 97, with 1 to 64
  channels; packets are 1 ms long up to 8 channels and 125 µs above, with
  no choice of packet time. A receiver refuses SDP files for other formats;
  without an SDP file it takes the stream for stereo and accepts any
  payload type.
- Changing a route's layout cuts its stream on the peer for a moment,
  while the receiver restarts.
- A receiver reads a stream's channel layout from its SDP file only, so
  channel labels, which only IS-04 carries, don't reach it; a label can be
  set per source instead.
- Wires have gains and meters, and an output that clips is warned
  about, but nothing guards it: there is no limiter, which would need
  Orpheus to do the summing itself.
- AES70 controllers see the wires and set their gains and mutes, but
  can't make or remove wires (see [future work](#aes70-wiring)). A pair
  has a gain of its own only where the wire names its pairs; nothing
  mutes a single pair. Meters show the peak only; the RMS is in the API.
  AES70 has no authentication and no OCP.1 over UDP.
- `when_lost = "pause"` pauses only MPRIS players whose process it can
  tell for sure, not those in a Flatpak sandbox or players without MPRIS;
  their streams are parked. Nothing resumes a paused player when its
  output is back. GNOME doesn't show the `(unavailable)` after a sink's
  name.
- GNOME's sound settings keep showing a parked stream on the output it
  lost, and a stream on the default output may show as playing on the
  default for up to 2 s after its own went.
- A stream on the default output follows the default the user chose
  (`default.configured.audio.sink`). Where nobody ever chose one, it
  follows WirePlumber's, which changes by itself when that device goes,
  and the stream moves along.
- At most `[receiver] max` (8, up to 32) streams per node; the next
  `connect` needs `--force`.
- The policy decides by sound, not by what a stream is: a stream of near
  silence (below -60 dBFS) counts as stopped, and background noise above
  it counts as playing.
- Ducking has two levels, `normal` and `high`. Priorities are set by the
  sender per connection, not by the receiving node.
- Only `POST /x-orpheus/v1/routes` creates senders; IS-05 can redirect,
  pause and resume them, not create them.
- IPv6 multicast groups are joined on the system's default interface, so a
  node without an IPv6 default route can't join them.
- A connection made on a receiver through IS-05 doesn't reach the sender,
  and the other way round; only `orpheusctl connect` (and `disconnect`) set
  up both ends.
- A receiver stays connected to a sender that vanished without
  disconnecting until the sending node is back and says so, or for good if
  it never comes back or is not an Orpheus node. It costs a receiver, not
  sound.
- A peer that changes its address is followed only while discovery sees
  it; one reached by URL or address is not.
- `connect` accepts a peer with an NMOS API but no `[receiver]` of the right
  format only with `--send-only`.
- The sender sizes its buffer from the configured block (`[audio]
  block_frames` or `cycle_ms`, or assumes 1024 frames) instead of reading
  the block the audio system really runs, and the status doesn't show it.
- The receiver path has two buffers in a row (its port, then PipeWire's
  cycle). Playing out directly in the realtime callback would save a cycle.
- A connected receiver's playback stream runs even when nothing is sent to
  it, which keeps the PipeWire graph awake.
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
  message rates are fixed.
- The PTP clock sits on the system clock, so an NTP step disturbs it.
- The servo's gate against held messages and its hold-back of single far
  measurements have been measured on one home Wi-Fi network only (2 nodes,
  one access point; see [clock](clock.md#ptp)). The gate's margin (0.5 ms)
  and windows (8 and 16 s) are still guesses. A sudden change of the path
  delay still makes the clock step for a few seconds. The node warns
  (`ptp_steps`).
- Over Wi-Fi, with software timestamps, two nodes can be expected to agree
  within 0.1 to 1 ms at best, plus a constant error where the access point
  is slower one way than the other. That is fine between rooms, but not
  for the two speakers of one stereo pair: the ear hears time differences
  between left and right from about 20 µs, and around 1 ms the image moves
  to the earlier speaker. A stereo pair needs both speakers wired, or both
  channels from one node, until better timestamps (in the driver, in
  hardware, or Wi-Fi FTM) are available.
- PTP needs `CAP_NET_BIND_SERVICE`, which has to be set again after every
  build.

**Control**

- The HTTP API has no authentication.
- NMOS: peer-to-peer discovery only; nodes don't register with an NMOS
  registry. Interfaces don't name the switch port they are attached to
  (`attached_network_device`).
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
  never settle, and sync between rooms suffers. The servo's gate keeps held
  messages out of the clock, and nodes warn about their own power saving
  (`power_save`). PTP messages also go by unicast to the other Orpheus
  nodes on Wi-Fi, which take them only that way: the access point neither
  holds unicast back for dozing clients nor sends it at its lowest rate;
  see [clock](clock.md#ipv4-and-ipv6).
  On a home network (2026-10-26) the clock settled within 40 s and stayed
  within 13 µs. Held multicast did not show there; what delayed messages
  by up to 120 ms were the background scans of a node with a weak signal,
  every 5 minutes. They also make audio late; see
  [troubleshooting](troubleshooting.md#dropouts-every-few-minutes-on-wi-fi).
  A grandmaster that is not an Orpheus node still reaches Wi-Fi nodes by
  multicast only. Planned: the Wi-Fi profile; maybe a warning when the
  node's Wi-Fi interface scans in the background.
- **IGMP/MLD snooping without a querier.** Managed switches, and some routers
  and access points, silently stop forwarding PTP multicast after a few
  minutes. Plan: detect it (no announce from the grandmaster for a while) and
  warn; it is described in [troubleshooting](troubleshooting.md).
- **Host firewalls.** firewalld and ufw block UDP 319, 320 and 5004 and TCP
  7480 by default, ufw also IGMP (so snooping stops multicast to the node), while mDNS often gets through, so peers show up but PTP
  and routes stay silent (seen in the first two-box test). The node can't
  read the rules without root. Plan:
  - Detect the symptoms: as grandmaster, followers but no delay requests;
    as follower, syncs but no delay responses; a peer that discovery lists
    whose API times out; a route whose peer receiver reports no packets.
  - A probe to tell which side blocks: a node asks a peer over the API to
    send one UDP packet to its port 319 or 5004; if the API works but the
    packet doesn't arrive, this node's inbound UDP is filtered.
  - Name the firewall without root (`/etc/ufw/ufw.conf` is world-readable,
    firewalld answers over D-Bus) and warn (`firewall`) with the exact
    command for it and the local network filled in.
  - The UI offers to fix it with the system's password dialog: `pkexec ufw
    …`, or firewalld's D-Bus API through polkit. The daemon never changes
    the firewall itself.
  - Packages ship a ufw application profile and a firewalld service file,
    so the fix is `ufw allow orpheus` or `--add-service=orpheus`. On
    Windows and macOS the system asks when a program first listens, and a
    Windows installer can add the rule.
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
  the capabilities on install; a system service could use systemd's
  `AmbientCapabilities`. `CAP_NET_ADMIN`, to turn Wi-Fi power saving off,
  allows far more than that; a small helper with only that job would be
  safer.

### Control plane and configuration

- **Split between config and control plane.** Routing lives in the API and
  is saved in the state directory (`routes.json`); `config.toml` holds
  defaults. Settings changed over the API override the file and show where
  each value comes from (done in M6, see
  [configuration](configuration.md#settings-changed-over-the-api)). Settings
  that need a restart (ports, `[receiver] max`, the audio block) can only
  be set in the file.
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

### AES70 wiring

Optional: wires made and removed over AES70, through an `OcaMatrix` per
output, its rows the channels of the inputs and sources there are, its
columns the output's channels, a gain at each crossing. Raising a
crossing from silence would make a wire, lowering it to silence remove
it. It can only show part of the model, so it waits for a controller
someone wants to wire from:

- A wire to a source that plays nowhere now has no row, and rows shift as
  sources come and go.
- Pairs named by symbol or label (`L=C`) become fixed channel numbers once
  set through the matrix.
- With no wires an input plays whole on its output; the matrix would show
  that as empty, or setting a crossing would turn it into explicit wires.

### Receiver pool and policies

Done in M5, see [usage](usage.md#several-streams). The receiving node
decides what it plays: a sender always connects to a free receiver, and
the node's policy only sets each stream's volume. Open ideas:

- Priorities and policies set by the receiving node per sending node or
  per source type (a doorbell always high, the TV never ducked), instead
  of only by the sender.
- More than two priority levels, and ducking by a configurable amount per
  connection.
- A policy by output: streams on different outputs of one node (kitchen
  speaker, headphones) need not compete.
- Sample-rate changes come with the AES67 extended mode (44.1, 96 kHz), as
  the native format is fixed.
- Remote volume: the sink's volume on the sending node and the stream's
  volume on the receiving node are independent today and multiply. An
  AirPlay-like mode could let the sink's slider set the receiver's volume
  instead, with the sink at 100%. To settle: which wins when both change,
  how it passes through relays hop by hop, and whether it is set per
  route or per node.

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
