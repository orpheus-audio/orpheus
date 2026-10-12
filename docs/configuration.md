# Configuration

## Files

A node keeps its files where the XDG base directory specification says:

```text
~/.config/orpheus/config.toml   configuration, optional, hand-edited
~/.local/state/orpheus/id       device ID, created on first start
~/.local/state/orpheus/         later also peers, aliases, routes and gains
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
# quantum = 256

[receiver]
enabled = true
port = 5004
link_offset_ms = 10
# target = "alsa_output.pci-0000_00_1f.3.analog-stereo"

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
and stop senders.

### `[audio]`

| Key | Default | Meaning |
|---|---|---|
| `quantum` | unset | Frames per PipeWire cycle to request for Orpheus streams. Smaller means lower latency and more CPU wakeups. Unset keeps the system default. The senders size their buffer from it and assume 1024 frames when it is unset. |

### `[receiver]`

Every node has one receiver. It plays the RTP stream sent to it on its output.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Run the receiver. |
| `port` | `5004` | UDP port for RTP, over IPv4 and IPv6. |
| `link_offset_ms` | `10` | Playout delay after the sender's timestamp. It must cover packetization (1 ms), network delay, and the clock error between the nodes. Every receiver of a stream should use the same value so that they play in sync. |
| `target` | unset | `node.name` of the PipeWire output to play on; the default output if unset. `pw-cli ls Node` or `wpctl status` lists the outputs. Temporary: this moves to the API (see below). |

The receiver follows one stream at a time. If a second sender streams to the
same node, it is ignored until the first one has been quiet for a second.

Choosing where streams play is routing, which belongs to the control plane.
`target` and `link_offset_ms` will therefore probably move to the API, and the
node will remember them in its state directory.

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
from the icon theme. Until discovery exists, every network sink is `generic`.

### `[loopback]`

A debug route, not a user feature: it creates a sink "Orpheus loopback" that
plays straight back on a local output through the engine. It is used to
measure the engine's own latency.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Create the loopback sink. |
| `latency_ms` | `20` | Buffer between the sink and the playback stream. |
| `target` | unset | `node.name` of the output; the default output if unset. |

## Environment

| Variable | Meaning |
|---|---|
| `ORPHEUS_HOME` | One directory for config, state and cache; see [files](#files). |
| `ORPHEUS_STATE`, `ORPHEUS_CACHE` | State and cache directory; they win over `ORPHEUS_HOME`. |
| `RUST_LOG` | Log levels, for example `debug` or `info,orpheus_clock=debug`. Default `info`. See [logs](troubleshooting.md#logs). |
| `ORPHEUS_NODE` | Node URL for `orpheusctl`; default `http://localhost:7480`. |
| `ORPHEUS_DEBUG_CLOCK_SKEW_PPM` | Testing only: run this node's clock fast by this many ppm. |
| `ORPHEUS_DEBUG_CLOCK_OFFSET_MS` | Testing only: offset this node's clock by this many milliseconds. |

The two debug variables let one machine play two nodes whose clocks disagree;
see [development](development.md).
