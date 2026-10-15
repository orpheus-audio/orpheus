# Configuration

## Files

A node keeps its files where the XDG base directory specification says:

```text
~/.config/orpheus/config.toml   configuration, optional, hand-edited
~/.local/state/orpheus/id       device ID, created on first start
~/.local/state/orpheus/routes.json  routes and connections, kept across restarts
~/.local/state/orpheus/outputs.json types set for outputs (`orpheusctl set-type`)
~/.local/state/orpheus/peers.json   aliases and pins of other nodes
~/.local/state/orpheus/sources.json output and volume of each source played here
~/.local/state/orpheus/settings.json settings changed over the API
~/.cache/orpheus/               anything that can be rebuilt (not used yet)
```

Each directory is taken from the first of these that is set:

| | Config | State | Cache |
|---|---|---|---|
| 1 | | `ORPHEUS_STATE` | `ORPHEUS_CACHE` |
| 2 | `ORPHEUS_HOME` | `ORPHEUS_HOME/state` | `ORPHEUS_HOME/cache` |
| 3 | `CONFIGURATION_DIRECTORY` | `STATE_DIRECTORY` | `CACHE_DIRECTORY` |
| 4 | `XDG_CONFIG_HOME/orpheus` | `XDG_STATE_HOME/orpheus` | `XDG_CACHE_HOME/orpheus` |
| 5 | `~/.config/orpheus` | `~/.local/state/orpheus` | `~/.cache/orpheus` |

`ORPHEUS_HOME` puts everything under one directory, for example to run a
second node on the same machine. Row 3 is set by systemd for a unit with
`ConfigurationDirectory=orpheus`, `StateDirectory=orpheus` and
`CacheDirectory=orpheus`. For a system service that means `/etc/orpheus`,
`/var/lib/orpheus` and `/var/cache/orpheus`. The log line `starting` shows
which directories a node uses.

The **device ID** is 8 characters of Crockford Base32, for example
`K7QM3XRA`. It is generated on first start and never changes. Delete `id` to
make the node a new device.

## `config.toml`

`config.toml` is optional, and every key has a default. Unknown keys are an
error, so typos don't go unnoticed. Changes take effect when `orpheusd`
restarts.

### Example

All values below are the defaults, except `name`.

```toml
name = "Living room"

[api]
listen = "[::]:7480"

[audio]
# block_frames = 256    # or cycle_ms = 5, not both

[receiver]
enabled = true
port = 5004
max = 8
link_offset_ms = 10
accept = "connected"
policy = "last"
duck_db = 20

[ptp]
enabled = true
# interface = "enp3s0"
transport = "auto"
domain = 0
priority1 = 250
priority2 = 128
slave_only = false
step_threshold_us = 1000
settle_threshold_us = 100
```

### Top level

| Key | Default | Meaning |
|---|---|---|
| `name` | hostname | Name the node announces. |

### `[api]`

| Key | Default | Meaning |
|---|---|---|
| `listen` | `[::]:7480` | Address and port of the HTTP API. `[::]` accepts IPv4 and IPv6; where IPv6 is disabled the node falls back to `0.0.0.0`. Use `127.0.0.1:7480` to keep the API local. |

The API has no authentication yet. Anyone who can reach the port can start
and stop routes.

### `[audio]`

The audio system moves audio in blocks: once per cycle it wakes up, and
every stream hands over or takes one block. Orpheus' streams copy each
block between the audio system and the network threads, so the block size
sets part of the latency, about one block in each direction.

| Key | Default | Meaning |
|---|---|---|
| `block_frames` | unset | Block size to ask for, in frames. The same number at every sample rate. |
| `cycle_ms` | unset | Block size to ask for, as a duration. It is converted to frames at the stream's sample rate and rounded to a whole frame, so the latency stays the same if the rate changes. |

Set one of them or neither; setting both is an error. The block must come
to 16 to 8192 frames, at 48 kHz 0.33 to 170 ms. Unset, the audio system
picks (PipeWire usually 1024 frames, 21 ms, unless other applications ask
for less).

- **Smaller blocks** mean lower latency, but more wakeups and CPU, and
  under load the audio system may not keep up (`underruns` in the
  [status](usage.md#reading-the-status)).
- **Larger blocks** cost latency but survive load.
- Use `block_frames` to match a number the audio system shows, such as
  PipeWire's quantum in `pw-top`; use `cycle_ms` to think in time.
- Senders size their buffer from the block (1024 frames if unset). A block
  the audio system makes smaller only leaves more headroom.

What each platform does with it:

| Platform | The block becomes |
|---|---|
| Linux, PipeWire | `node.latency = "<frames>/<rate>"` on Orpheus' streams, a request. PipeWire runs the whole graph at the smallest latency any stream asks for, scaled to the graph's rate, limited by `default.clock.min-quantum` and `max-quantum` (32 and 2048 by default), and rounded down to a power of two unless `clock.power-of-two-quantum` is off. So `cycle_ms = 5` (240 frames) runs at 128 frames, 2.7 ms. `pw-top` shows the real quantum (`QUANT`). |
| Linux, ALSA (planned) | The period size; the buffer holds two to three periods. |
| Windows, WASAPI (planned) | The buffer duration. Shared mode runs at the audio engine's period (usually 10 ms), or with `IAudioClient3` at a multiple of the device's smallest period; exclusive mode takes the block as the period. |
| macOS, Core Audio (planned) | The device's buffer frame size (`kAudioDevicePropertyBufferFrameSize`), within the range the device allows. |
| ESP32 (planned) | The I2S DMA buffer length in frames (`dma_frame_num`). |

Where a platform can't run the exact size, it takes the nearest it can;
`orpheusctl status` will show the size in use once nodes read it back (see
the [roadmap](roadmap.md#known-limitations)).

### `[receiver]`

A node has a receiver per connection and one free receiver more, so that
another node can always connect. Each plays the RTP stream sent to it on
an output; see [several streams](usage.md#several-streams).

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Run receivers. |
| `port` | `5004` | UDP port of the first receiver, over IPv4 and IPv6. Receiver *k* listens on `port + 2k`, unless a connection asks for another port. |
| `max` | `8` | Most receivers, and so most streams played at once: 1 to 32. With the defaults the receivers use ports 5004 to 5018. Once `max` receivers are connected, there is no free one, and another node can connect only with `--force`. |
| `link_offset_ms` | `10` | Playout delay after the sender's timestamp. It must cover packetization (1 ms), network delay, and the clock error between the nodes. Every receiver of a stream should use the same value so that they play in sync. |
| `accept` | `"connected"` | What the free receiver plays: `"connected"` plays nothing, `"any"` plays any stream sent to its port. |
| `policy` | `"last"` | Which streams are heard when several play at once: `"last"` the one that started most recently, `"first"` the one that started first, `"mix"` all. See [several streams](usage.md#several-streams). |
| `duck_db` | `20` | How far the other streams are turned down while a high-priority stream plays, in dB: 0 to 96. |
| `default_output` | unset | Machine name of the output (see `orpheusctl outputs --json`) for streams whose source has no output [remembered](usage.md#outputs-and-volume). Unset: the audio system's default output, unless that is a sink to another node. |

Orpheus connects both ends of a stream: `orpheusctl connect` connects the
peer's receiver to the new sender through [IS-05](usage.md#connections-is-05),
and the receiver then plays only that sender. With `accept = "any"` a
receiver without a connection also plays streams that nobody connected,
for example from `orpheusctl connect --send-only` or an AES67 device sending
to it. Such a receiver follows one stream at a time: a second sender is
ignored until the first has been quiet for a second.

Receivers play on the default output, or on `default_output`. To play a
stream on another one, move it (named after the sending node, else
"Orpheus") in `pavucontrol`, with `wpctl` or with `orpheusctl play`; the
node remembers the output and the volume per
[source](usage.md#outputs-and-volume). Other nodes are told what kind of
device that output is; see [output types](usage.md#output-types). Types
are set per output over the API, not in this file.

Which streams go where is routing, which belongs to the API, not to this
file: routes and connections are kept in `routes.json` in the state
directory and restored at start.

### `[routes]`

How long a disconnected [route](usage.md#routes) is remembered, so that
connecting the same peer again brings it back as it was. Routes to
[pinned](usage.md#aliases-and-pins) nodes are kept regardless.

| Key | Default | Meaning |
|---|---|---|
| `max_age_days` | `365` | Forget a route this many days after it last ran. |
| `max_entries` | `100` | Forget the oldest routes beyond this many. |

### `[sources]`

How long the receivers remember the output and volume of a
[source](usage.md#outputs-and-volume) after it last played. Sources from
[pinned](usage.md#aliases-and-pins) nodes and those playing are kept
regardless.

| Key | Default | Meaning |
|---|---|---|
| `max_age_days` | `365` | Forget a source this many days after it last played. |
| `max_entries` | `100` | Forget the oldest sources beyond this many. |

### `[ptp]`

Clock synchronization with the other nodes and with AES67 devices. See
[clock](clock.md) for how it works.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Run PTP. When disabled, or when PTP cannot start, the node uses the system clock. |
| `interface` | unset | Network interface for PTP. Unset picks the interface of the IPv4 default route, or of the IPv6 default route if there is no IPv4 one. |
| `transport` | `"auto"` | `"ipv4"`, `"ipv6"` or `"auto"`. Auto uses IPv4 if the interface has an IPv4 address, IPv6 otherwise. Nodes on different transports don't see each other. AES67 and Dante devices only speak IPv4. |
| `domain` | `0` | PTP domain. Only nodes in the same domain synchronize. |
| `priority1` | `250` | Grandmaster election; lower wins. The default lets dedicated grandmasters (usually 128 or less) take precedence over nodes. |
| `priority2` | `128` | Tie-breaker in the election. |
| `slave_only` | `false` | Never become the grandmaster. |
| `step_threshold_us` | `1000` | Offsets above this are corrected in one step, smaller ones gradually. |
| `settle_threshold_us` | `100` | The clock counts as settled once its offset from the grandmaster stays below this. |

The PTP message rates follow the AES67 media profile and cannot be configured
yet: sync every 125 ms, announce every 2 s, delay requests every second.

### `[icons]`

Linux only. Icons for other nodes' outputs, by output type, as XDG icon
names. Each key replaces one default; the others stay.

```toml
[icons]
hifi = "audio-card"
generic = "audio-speakers"
```

| Type | Default icon |
|---|---|
| `speaker` | `orpheus-speaker` |
| `headphones` | `audio-headphones` |
| `headset` | `audio-headset` |
| `tv` | `video-display` |
| `hifi` | `orpheus-hifi` |
| `computer` | `computer` |
| `portable` | `multimedia-player` |
| `phone` | `phone` |
| `generic` | `orpheus-network-speaker` |

The `orpheus-*` icons ship with Orpheus, see
[`orpheusd/dist/icons`](../orpheusd/dist/icons/README.md); the others come
from the icon theme. A sink to another node takes the type its receiver
announces; see [output types](usage.md#output-types).

### `[loopback]`

A debug route, not a user feature: it creates a sink "Orpheus loopback" that
plays straight back on the default output through the engine. It is used to
measure the engine's own latency.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Create the loopback sink. |
| `latency_ms` | `20` | Buffer between the sink and the playback stream. |

## Environment

| Variable | Meaning |
|---|---|
| `ORPHEUS_HOME` | One directory for config, state and cache; see [files](#files). |
| `ORPHEUS_STATE`, `ORPHEUS_CACHE` | State and cache directory; they win over `ORPHEUS_HOME`. |
| `RUST_LOG` | Log levels, for example `debug` or `info,orpheus_clock=debug`. Default `info,statime=warn`. See [logs](troubleshooting.md#logs). |
| `ORPHEUS_NODE` | Node URL for `orpheusctl`; default `http://localhost:7480`. |
| `ORPHEUS_DEBUG_CLOCK_SKEW_PPM` | Testing only: run this node's clock fast by this many ppm. |
| `ORPHEUS_DEBUG_CLOCK_OFFSET_MS` | Testing only: offset this node's clock by this many milliseconds. |
| `ORPHEUS_DEBUG_WIRELESS` | Testing only: interfaces to treat as Wi-Fi, separated by commas, or `*` for all. |
| `ORPHEUS_DEBUG_NO_SEND_TIMESTAMPS` | Testing only: stamp PTP messages before sending, as on an interface without send timestamps; see [clock](clock.md#ptp). |

The clock variables let one machine play two nodes whose clocks disagree,
and `ORPHEUS_DEBUG_WIRELESS` tests the [network
warnings](usage.md#warnings) without Wi-Fi; see
[development](development.md).
