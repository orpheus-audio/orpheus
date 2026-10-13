# Usage

## `orpheusctl`

`orpheusctl` controls any node over its HTTP API. It talks to
`http://localhost:7480` unless told otherwise:

```sh
orpheusctl --node http://kitchen.local:7480 status
ORPHEUS_NODE=http://[fd00::20]:7480 orpheusctl status
```

| Command | Effect |
|---|---|
| `status` | Shows the node's ID, name, version, clock, receiver and senders. |
| `status --watch [SECONDS]` | Refreshes the status every second, or every `SECONDS`, until interrupted. Counters are shown as rates, for example `3 late/s`. |
| `nodes` | Lists the other nodes the node found on the network, with ID, address and version. Nodes that stopped stay listed as offline. |
| `connect PEER [--name NAME] [--force]` | Sends the node's audio to `PEER` and connects a free receiver there to it (see [connecting](#connecting)). This creates a sink called `NAME` (the peer's name if none is given), with an icon for the peer's kind of output. Whatever is played into the sink comes out of the peer. The connection is kept across restarts. |
| `connect PEER --send-only [--name NAME]` | Only sends; the peer's receiver is left alone. For devices without NMOS, and receivers set to accept any stream. |
| `disconnect ID` | Stops a sender, removes its sink and disconnects the peer's receiver. The ID is shown by `connect` and `status`. |
| `outputs` | Lists the node's outputs with their [types](#output-types), and which one the receiver plays on. |
| `set-type OUTPUT TYPE` | Sets what kind of device an output is; `auto` goes back to the detected type. `OUTPUT` is the name or description from `outputs`, or a part of either that only one output has. |

`--json` prints the node's JSON responses instead of text: the status as the
[HTTP API](#http-api) returns it, the node list, the outputs, and after `connect` the new sender (`id`, `name`,
`destination`, and the peer's `receiver`). Scripts
should use it, because the text output may change. With `--watch` it prints
one line per sample:

```sh
orpheusctl --json status | jq .clock.ptp.offset_ns
orpheusctl --json status --watch 5 >> status.jsonl
```

### Connecting

A connection has two ends, and `connect` sets up both. It finds the peer,
asks it for a receiver that has no sender, starts a sender to that
receiver's port, and connects the receiver to the sender through the peer's
[IS-05 API](#connections-is-05), handing it the sender's SDP file. If the
receiver refuses, the sender is removed again. `disconnect` undoes both
ends; the peer's receiver is left alone if another sender has taken it
over meanwhile, and the sender goes even if the peer can't be reached.

`PEER` is one of:

```text
Kitchen                  a name, as `orpheusctl nodes` lists it
TAPXPNPX                 a node ID
kitchen.local            a host name or address; its API on port 7480
192.168.1.20
http://192.168.1.20:7481 the URL of the node's API
```

If the peer's receiver plays another sender, `connect` fails with `409`
and names the holder; `--force` takes the receiver over. The receiver's
playback stream is named after the sending node, so mixers show where the
audio comes from.

A connected sender follows its peer's receiver. Each node announces a
change counter for its receivers over mDNS; when the peer's changes, or
every 30 seconds anyway, the sender asks the receiver again, so changes
arrive within a few seconds:

- The sink shows the peer's [output type](#output-types).
- The sink is renamed when the peer is, unless `--name` named it.
- If someone else connects the receiver, the sender pauses and `status`
  shows it as `disconnected, paused`. It resumes when the receiver is
  connected to it again, for example over IS-05; `disconnect` removes it.
- While the peer is offline or doesn't answer, `status` shows the sender as
  `unreachable`. It keeps sending, so the audio is back when the peer is.

With `--send-only`, `PEER` is a host name or address with an optional
port (default 5004). IPv6 addresses with a port need brackets:

```text
kitchen.local            192.168.1.20:5006
fd00::20                 [fd00::20]:5006
fe80::20%enp3s0          [fe80::20%enp3s0]:5006
```

A receiver plays only the sender connected to it, unless it is set to
[accept any stream](configuration.md#receiver).

A host name that resolves to both IPv4 and IPv6 is reached over IPv4, which
every receiver listens on.

Senders are not saved. They end when `orpheusd` stops.

## Reading the status

```text
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

Receiver       ● receiving ssrc 5e1f02aa
  listen       [::]:5004
  link offset  10 ms
  margin       8.71 ms
  packets      120000
  buffer       1140 frames
  ASRC         -3.2 ppm
  problems     none

Senders
  ID  NAME     PEER               PACKETS       BUFFER      ASRC  PROBLEMS
  0   Kitchen  192.168.1.20:5004   120000  1120 frames  +8.9 ppm  none
```

On a terminal the dots are coloured: green is fine, yellow needs attention
(settling, on the system clock), red is broken. Problem counters, a
`margin` below 1 ms and more than two `steps` are highlighted too. Colour is
off when the output is piped or `NO_COLOR` is set.

Counters are totals since the start. With `--watch` they are rates over the
last interval instead, so `problems` shows only what is going wrong now.

**Clock**

- `PTP, settled` or `PTP, settling`: see [clock](clock.md#ptp).
  `system clock` means PTP is off or could not start; the log says why.
- `interface`: the interface and transport PTP uses.
- `state`: `slave` follows the grandmaster, `master` is it. `listening` means
  no other PTP clock has been heard yet. `uncalibrated` means following but
  not yet synchronized.
- `grandmaster`: the clock everyone follows; `this node` if it is this one.
- `offset`: estimated difference from the grandmaster. A few µs to tens of µs
  is normal with software timestamps.
- `delay`: estimated one-way network delay to the grandmaster.
- `frequency`: how much faster than the system clock the PTP clock runs. This
  is the crystal difference between the two machines, typically within
  ±50 ppm.
- `steps`: how often the clock was stepped. One or two after a start are
  normal; more later means the clock is jumping.

**Receiver**

- `receiving from …`: the connected sender, by its node's name, and
  audio arrives; `connected to …, idle` if nothing arrives.
  `receiving ssrc …` is an unconnected receiver that accepts any stream.
  `not connected` means no sender is connected and the receiver plays
  nothing; `idle` that it would play any stream but none arrives; `off`
  that a [connection](#connections-is-05) switched it off.
- `accepts`: `connected senders`, or `any sender` (see
  [configuration](configuration.md#receiver)).
- `output`: the kind of output it plays on, which other nodes show, and
  that output (see [output types](#output-types)).
- `ignored`: packets turned away because they are not from the connected
  sender, or the receiver plays nothing. Shown only when there are any.
- `margin`: the smallest time between a packet's arrival and its playout over
  the last second. It should stay well above zero. If it gets close to zero,
  raise `link_offset_ms`.
- `problems`: the counters that are not zero, all counted since the start:
  - `lost`: packets that never arrived. `late`: arrived after their playout
    time. Both cause short gaps.
  - `early`: arrived too far ahead to buffer, which usually means the clocks
    disagree.
  - `malformed`: not RTP, or not the expected format.
  - `realigns`: the thread stalled or the clock jumped by more than 100 ms.
  - buffer problems, see below.

**Senders** have the same buffer columns, and `send errors` among their
problems: packets the operating system could not send, for example because
there is no route to the peer. `(off)` after the peer means a connection
paused the sender: its sink still takes audio, but nothing is sent.

**Buffers** (receiver, senders, loopback)

- `buffer`: fill level of the ring between PipeWire and the network thread.
  In both directions it is about one quantum plus a few milliseconds:
  PipeWire moves a whole cycle at a time, the network threads 1 ms.
- `ASRC`: the current rate correction between the media clock and the sound
  card. It settles near the sound card's crystal error.
- `underruns`: the ring ran dry, so there was a gap.
- `overruns`: the ring was full and audio was dropped.
- `resyncs`: audio was dropped to bring the latency back to target.

## HTTP API

Orpheus' own API lives under `/x-orpheus/v1`, the NMOS APIs under `/x-nmos`
(see [NMOS](#nmos)).

| Method and path | Effect |
|---|---|
| `GET /x-orpheus/v1/status` | Node status as JSON (what `orpheusctl status` shows). |
| `POST /x-orpheus/v1/senders` | Starts a sender and [connects](#connecting) a receiver of the peer to it. Body: `{"peer": "Kitchen"}`, optionally with `"name"`, `"force": true` or `"send_only": true`. Returns `201` with `{"id": 0, "name": "Kitchen", "destination": "192.168.1.20:5004", "receiver": "<uuid>"}`; `409` if the peer has no free receiver, `502` if it can't be reached or refuses. |
| `DELETE /x-orpheus/v1/senders/{id}` | Stops a sender and disconnects the peer's receiver. Returns `204`, or `404` if there is no such sender. |
| `GET /x-orpheus/v1/senders/{id}/sdp` | The sender's stream as an SDP file (`application/sdp`). |
| `GET /x-orpheus/v1/outputs` | The outputs: `name`, `description`, `type`, `detected`, `override` (if set), `default`, and `receiver` if the receiver plays there. |
| `PUT /x-orpheus/v1/outputs/{name}` | Sets an output's type. Body: `{"type": "hifi"}`. Returns `204`, or `404` if there is no such output. |
| `DELETE /x-orpheus/v1/outputs/{name}` | Goes back to the detected type. Returns `204`, or `404` if no type was set. |
| `GET /x-orpheus/v1/nodes` | The other nodes found over mDNS (what `orpheusctl nodes` shows). Returns `503` if mDNS could not start. |
| `GET /x-orpheus/v1/openapi.json` | The OpenAPI 3.1 description of the API. |

The same OpenAPI document is printed by `orpheusd --openapi`.

```sh
curl -s localhost:7480/x-orpheus/v1/status | jq .clock
curl -s -X POST localhost:7480/x-orpheus/v1/senders \
  -H 'content-type: application/json' -d '{"peer": "Kitchen"}'
```

## Output types

Every output has a type: `speaker`, `headphones`, `headset`, `tv`, `hifi`,
`computer`, `portable`, `phone` or `generic`. A node announces the type of
the output its receiver plays on, and nodes connected to it show their sink
to it with a matching icon and PipeWire form factor.

- **Which output:** the one the receiver's stream is linked to in PipeWire,
  so moving the stream in `pavucontrol` changes the announced type; while
  it is not linked, the default output.
- **Detected type:** PipeWire's form factor of the output (`internal` counts
  as `computer`), or `tv` for HDMI and DisplayPort profiles, else
  `generic`. A sink to another node has the type that node's receiver
  announces; senders [follow it](#connecting).
- **Set type:** `orpheusctl set-type` overrides the detected type of one
  output, kept in `outputs.json` in the [state
  directory](configuration.md#files). Use it where detection is wrong, for
  example a line output that feeds an amplifier:

  ```sh
  orpheusctl outputs
  orpheusctl set-type "Built-in Audio" hifi
  orpheusctl set-type Kitchen tv      # a sink to another node, here only
  orpheusctl set-type Kitchen auto
  ```

Because a sink to another node is an output like any other, types pass
along chains: if node B plays what it receives into its sink to node C, B
announces C's type, unless B has a type set for that sink.

## Discovery

Every node announces its HTTP API over mDNS as an NMOS node
(`_nmos-node._tcp`), under its name and as the host
`orpheus-<id>.local`. Other NMOS nodes show up in `orpheusctl nodes` too,
without an ID. To see the announcements with Avahi:

```sh
avahi-browse -rt _nmos-node._tcp
```

If two nodes have the same name, the second announces itself as
`Name (2)`, and `orpheusctl nodes` shows it that way.

## NMOS

Each node serves the NMOS IS-04 Node API v1.3 at `/x-nmos/node/v1.3/`, so NMOS
controllers can see it. It describes the node as one device with:

- one **receiver**, the node's RTP receiver;
- per sender started with `connect`, a **source**, a **flow** (L24, 48 kHz,
  stereo) and a **sender**.

```sh
curl -s localhost:7480/x-nmos/node/v1.3/self | jq
curl -s localhost:7480/x-nmos/node/v1.3/senders | jq '.[].label'
```

Each sender's `manifest_href` points to its SDP file, which describes the
stream the way AES67 and SMPTE ST 2110-30 receivers expect: destination,
L24/48000/2, payload type 97, 1 ms packets, the PTP grandmaster and domain
(`ts-refclk`) and `mediaclk:direct=0`:

```sh
curl -s localhost:7480/x-orpheus/v1/senders/0/sdp
```

Without PTP the SDP names the sender's own clock (`ts-refclk:localmac=…`).
The session name (`s=`) is the node's name; receivers name their playback
stream after it.

Resource IDs are derived from the node ID, so they stay the same across
restarts. The receiver carries the tag `urn:x-orpheus:form-factor` with the
[type](#output-types) of the output it plays on. Nodes don't register with
an NMOS registry; they only use peer-to-peer discovery.

### Connections (IS-05)

The IS-05 Connection API v1.1 at `/x-nmos/connection/v1.1/` routes the
senders and the receiver. Each has **staged** parameters, which a controller
changes with `PATCH`, and **active** ones, which the node runs. Staged
parameters become active at once (`activate_immediate`) or at a TAI time
(`activate_scheduled_absolute`, `activate_scheduled_relative`). `auto`
values are resolved in the active parameters.

- **Senders:** `destination_ip` and `destination_port` (unicast, or a
  multicast group for [interop](#multicast-interop)), `source_ip` and
  `source_port`. `master_enable: false`
  pauses the sender. The SDP file is at `…/senders/{id}/transportfile`.
- **Receiver:** a `transport_file` (the sender's SDP) connects it to that
  sender: it then only plays packets from the sender's address with its
  payload type, and joins its multicast group if it has one. `source_ip`,
  `multicast_ip` and `destination_port` can also be set directly.
  `master_enable: false` switches it off. Without a source address or
  group it plays nothing, or, if it [accepts any
  stream](configuration.md#receiver), whatever arrives on its port.

For example, to connect this node's receiver to a sender of another node:

```sh
C=localhost:7480/x-nmos/connection/v1.1/single
RX=$(curl -s $C/receivers/ | jq -r '.[0]' | tr -d /)
SDP=$(curl -s 192.168.1.20:7480/x-nmos/connection/v1.1/single/senders/<id>/transportfile)
jq -n --arg sdp "$SDP" '{master_enable: true,
  activation: {mode: "activate_immediate"},
  transport_file: {data: $sdp, type: "application/sdp"}}' |
  curl -s -X PATCH $C/receivers/$RX/staged -H 'content-type: application/json' -d @-
```

`orpheusctl connect` does the same from the sending node, after
activating its sender's destination. The node saves what was
last activated, as requested (with `auto`), in `routes.json` in its
[state directory](configuration.md#files) and activates it again at start.

### Multicast (interop)

Orpheus nodes stream to each other by unicast only; `orpheusctl connect`
never uses multicast. Multicast is there for other equipment: AES67 and
Dante devices usually send only to multicast groups, and studio NMOS
controllers expect it. A node uses it only when a connection asks:

- a receiver given an SDP file with a multicast address joins that group;
- a sender given a multicast `destination_ip` sends to that group (TTL 32).

Use it on wired networks whose switches do IGMP snooping. Switches without
it send the stream (about 2.6 Mbit/s) to every port, and over Wi-Fi it is
sent at the slowest rate to every client and can take a large part of the
airtime.

The API has no authentication yet. Restrict `[api] listen` to `127.0.0.1` if
the network is not trusted.
