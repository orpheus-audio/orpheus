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
| `connect PEER [--name NAME]` | Sends the node's audio to `PEER`. This creates a sink called `NAME` (the peer if no name is given), marked with a network icon. Whatever is played into the sink goes to the peer's receiver. |
| `disconnect ID` | Stops a sender and removes its sink. The ID is shown by `connect` and `status`. |

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
B73AQ0P0  desk  (orpheusd 0.0.1)
clock: PTP on enp3s0/ipv4 (4cb04a.fffe.50a582), slave, grandmaster 9c6b00.fffe.1a2b3c, offset -0.4 µs, delay 41.2 µs, +12.31 ppm, 1 steps, settled
receiver [::]:5004 (ssrc 5e1f02aa, link offset 10 ms): 120000 packets, 0 lost, 0 late, 0 early, 0 malformed, 0 realigns, margin 8.71 ms
  playout: 1140 frames buffered, 0 underruns, 0 overruns, 0 resyncs, ASRC -3.2 ppm
sender 0 → 192.168.1.20:5004: 120000 packets, 0 send errors, 0 realigns
  input: 1120 frames buffered, 0 underruns, 0 overruns, 0 resyncs, ASRC +8.9 ppm
```

**Clock line**

- `PTP on enp3s0/ipv4`: the interface and transport PTP uses. `clock: system`
  means PTP is off or could not start; the log says why.
- `slave` or `master`: following the grandmaster, or being it. `listening`
  means no other PTP clock has been heard yet. `uncalibrated` means following
  but not yet synchronized.
- `offset`: estimated difference from the grandmaster. A few µs to tens of µs
  is normal with software timestamps.
- `delay`: estimated one-way network delay to the grandmaster.
- `ppm`: how much faster than the system clock the PTP clock runs. This is the
  crystal difference between the two machines, typically within ±50 ppm.
- `steps`: how often the clock was stepped. One or two after a start is
  normal; more later means the clock is jumping.
- `settled` or `settling`: see [clock](clock.md#ptp).

**Receiver**

- `ssrc`: the stream being played; `idle` if none.
- `lost`: packets that never arrived. `late`: arrived after their playout
  time. Both cause short gaps.
- `early`: arrived too far ahead to buffer, which usually means the clocks
  disagree.
- `malformed`: not RTP, or not the expected format.
- `margin`: the smallest time between a packet's arrival and its playout over
  the last second. It should stay well above zero. If it gets close to zero,
  raise `link_offset_ms`.
- `realigns`: the thread stalled or the clock jumped by more than 100 ms.

**Buffers** (`playout`, `input`, `loopback`)

- `frames buffered`: fill level of the ring between PipeWire and the network
  thread. In both directions it is about one quantum plus a few milliseconds:
  PipeWire moves a whole cycle at a time, the network threads 1 ms.
- `underruns`: the ring ran dry, so there was a gap.
- `overruns`: the ring was full and audio was dropped.
- `resyncs`: audio was dropped to bring the latency back to target.
- `ASRC`: the current rate correction between the media clock and the sound
  card. It settles near the sound card's crystal error.

## HTTP API

The API lives under `/x-orpheus/v1`. The NMOS APIs will be added next to it.

| Method and path | Effect |
|---|---|
| `GET /x-orpheus/v1/status` | Node status as JSON (what `orpheusctl status` shows). |
| `POST /x-orpheus/v1/senders` | Starts a sender. Body: `{"peer": "192.168.1.20", "name": "Kitchen"}`; `name` is optional. Returns `201` with `{"id": 0}`. |
| `DELETE /x-orpheus/v1/senders/{id}` | Stops a sender. Returns `204`, or `404` if there is no such sender. |
| `GET /x-orpheus/v1/openapi.json` | The OpenAPI 3.1 description of the API. |

The same OpenAPI document is printed by `orpheusd --openapi`.

```sh
curl -s localhost:7480/x-orpheus/v1/status | jq .clock
curl -s -X POST localhost:7480/x-orpheus/v1/senders \
  -H 'content-type: application/json' -d '{"peer": "192.168.1.20"}'
```

The API has no authentication yet. Restrict `[api] listen` to `127.0.0.1` if
the network is not trusted.
