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
| `connect PEER [--name NAME]` | Sends the node's audio to `PEER`. This creates a sink called `NAME` (the peer if no name is given), marked with a network icon. Whatever is played into the sink goes to the peer's receiver. |
| `disconnect ID` | Stops a sender and removes its sink. The ID is shown by `connect` and `status`. |

`--json` prints the node's JSON responses instead of text: the status as the
[HTTP API](#http-api) returns it, the node list, and `{"id": 0}` after
`connect`. Scripts
should use it, because the text output may change. With `--watch` it prints
one line per sample:

```sh
orpheusctl --json status | jq .clock.ptp.offset_ns
orpheusctl --json status --watch 5 >> status.jsonl
```

`PEER` is a host name or address with an optional port; the default port is
5004. IPv6 addresses with a port need brackets:

```text
kitchen.local            192.168.1.20:5006
fd00::20                 [fd00::20]:5006
fe80::20%enp3s0          [fe80::20%enp3s0]:5006
```

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
  ID  PEER               PACKETS       BUFFER      ASRC  PROBLEMS
  0   192.168.1.20:5004   120000  1120 frames  +8.9 ppm  none
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

- `receiving ssrc …`: the stream being played; `idle` if none.
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
there is no route to the peer.

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

The API lives under `/x-orpheus/v1`. The NMOS APIs will be added next to it.

| Method and path | Effect |
|---|---|
| `GET /x-orpheus/v1/status` | Node status as JSON (what `orpheusctl status` shows). |
| `POST /x-orpheus/v1/senders` | Starts a sender. Body: `{"peer": "192.168.1.20", "name": "Kitchen"}`; `name` is optional. Returns `201` with `{"id": 0}`. |
| `DELETE /x-orpheus/v1/senders/{id}` | Stops a sender. Returns `204`, or `404` if there is no such sender. |
| `GET /x-orpheus/v1/nodes` | The other nodes found over mDNS (what `orpheusctl nodes` shows). Returns `503` if mDNS could not start. |
| `GET /x-orpheus/v1/openapi.json` | The OpenAPI 3.1 description of the API. |

The same OpenAPI document is printed by `orpheusd --openapi`.

```sh
curl -s localhost:7480/x-orpheus/v1/status | jq .clock
curl -s -X POST localhost:7480/x-orpheus/v1/senders \
  -H 'content-type: application/json' -d '{"peer": "192.168.1.20"}'
```

## Discovery

Every node announces its HTTP API over mDNS as an NMOS node
(`_nmos-node._tcp`), under its name and as the host
`orpheus-<id>.local`. Other NMOS nodes show up in `orpheusctl nodes` too,
without an ID. To see the announcements with Avahi:

```sh
avahi-browse -rt _nmos-node._tcp
```

If two nodes have the same name, the second announces itself as
`Name (2)`; `orpheusctl nodes` still shows the configured name.

The API has no authentication yet. Restrict `[api] listen` to `127.0.0.1` if
the network is not trusted.
