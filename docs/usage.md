# Usage

## `orpheusctl`

`orpheusctl` controls any node over its HTTP API. It talks to the node on
this machine unless told otherwise. `--node` takes a host name or address,
whose API is on port 7480, or the URL of the API:

```sh
orpheusctl --node kitchen.local status
orpheusctl status --node 192.168.1.20:7481
ORPHEUS_NODE=http://[fd00::20]:7480 orpheusctl status
```

| Command | Effect |
|---|---|
| `status` | Shows the node's ID, name, version, clock, receivers and routes. |
| `status --watch [SECONDS]` | Refreshes the status every second, or every `SECONDS`, until interrupted. Counters are shown as rates, for example `3 late/s`. |
| `nodes` | Lists the other nodes the node found on the network, with ID, address, version and link (`wired` or `Wi-Fi`, as each node announces it). Nodes that stopped stay listed as offline. A node with an [alias](#aliases-and-pins) is shown as `Alias (Name)`. |
| `connect PEER [--name NAME] [--priority high] [--force]` | Sends the node's audio to `PEER` and connects a free receiver there to it (see [connecting](#connecting)). This creates a route: a sink called `NAME` (the route's name from before, else the peer's name), with an icon for the peer's kind of output. Whatever is played into the sink comes out of the peer. `--priority high` plays over the peer's other streams (see [several streams](#several-streams)). The route is kept across restarts. By default it holds the peer's receiver all the time; it can also [claim it only while audio plays](#claiming-a-receiver-on-play). |
| `connect PEER --send-only [--name NAME]` | Only sends; the peer's receiver is left alone. For devices without NMOS, and receivers set to accept any stream. |
| `routes` | Lists the routes: the ones that run, with how they stand (`connected`, `idle`, `waiting for a free receiver`, `peer offline`, …), and the ones [remembered](#routes) from before. |
| `disconnect ROUTE` | Stops a route, removes its sink and disconnects the peer's receiver. The route is remembered. `ROUTE` is its name, or its ID or the start of it, as `routes` shows. |
| `forget ROUTE` | Forgets a route, so that the next `connect` starts a new one; a route that runs is disconnected first. |
| `alias NODE ALIAS` | Gives another node a name of your own, shown instead of the one it announces (see [aliases and pins](#aliases-and-pins)). `alias NODE --remove` removes it. |
| `pin NODE`, `unpin NODE` | Keeps another node listed while it is offline, also after a restart, or stops doing so. |
| `outputs` | Lists the node's outputs with their [types](#output-types) and [channels](#channel-layouts), which receivers play on each, and where [created outputs](#inputs-and-outputs) play. |
| `inputs` | Lists the node's [inputs](#inputs-and-outputs): the receivers' streams and the audio system's inputs, such as microphones, with their channels. |
| `output create NAME DEVICE CHANNEL… [--layout LAYOUT]` | Creates an output that plays on chosen channels of another, for example channels 3 and 4 of an audio interface (see [inputs and outputs](#inputs-and-outputs)). Kept across restarts. |
| `output remove OUTPUT` | Removes a created output. |
| `wire FROM OUTPUT [OUT=IN…]` | Plays channels of a source or an input on an output (see [wires](#wires)): `L=R R=L` plays the input's right channel on the output's left and the other way round; without pairs, all channels go where they belong. A source with wires plays only on them. Kept across restarts. |
| `unwire FROM OUTPUT [OUT=IN…]` | Removes a wire, or some of its channel pairs. A source left without wires is [parked](#lost-outputs) until played or wired again. |
| `patch` | Shows the wires as a patch bay: for each output channel, everything that plays on it. |
| `play RECEIVER OUTPUT` | Plays a receiver's stream on `OUTPUT` (as for `set-type`), or with `default` where streams play by default; remembered for its [source](#outputs-and-volume). |
| `default OUTPUT` | Makes `OUTPUT` (as for `set-type`) the default output, where applications play and received streams without an output of their own go; a route's sink works too, so that the node's audio goes to the peer. The same as choosing it in the desktop's sound settings, for nodes without a desktop. The audio system keeps it across restarts. |
| `sources` | Lists the [sources](#outputs-and-volume) the receivers remember: where each plays, how loud, and whether it plays now. |
| `forget-source SOURCE` | Forgets where a source plays and how loud, and its layout; one that plays now goes to the default output at full volume. `SOURCE` is its name, or its ID or the start of it, as `sources` shows. |
| `layout SOURCE LAYOUT` | Sets what the channels of a source carry, for senders that say nothing or say it wrong (see [channel layouts](#channel-layouts)): ST 2110-30 groups such as `ST`, `DM` or `51`, or channel symbols such as `L R`; `auto` goes back to what the stream says. |
| `settings` | Shows the [settings](configuration.md#settings-changed-over-the-api) that can change while the node runs, each with where its value comes from: `default`, `config` (config.toml) or `api`. |
| `set SECTION.KEY=VALUE…` | Changes settings while the node runs, for example `set receiver.policy=mix ptp.domain=1`; kept across restarts, config.toml is not touched. A bad value changes nothing. |
| `reset SECTION.KEY…` | Undoes `set`: the value from config.toml or the default applies again. `reset ptp` resets a whole section. |
| `set-type OUTPUT TYPE` | Sets what kind of device an output is; `auto` goes back to the detected type. `OUTPUT` is the name or description from `outputs`, or a part of either that only one output has. |

`--json` prints the node's JSON responses instead of text: the status as the
[HTTP API](#http-api) returns it, the node list, the outputs, the routes, and after `connect` the route (`id`, `name`,
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
[IS-05 API](#connections-is-05), handing it the sender's SDP file. Then it
asks the receiver whether it really plays the new sender: if another node
connected it at the same moment, `connect` tries once more with another
free receiver. If the receiver refuses, the sender is removed again.
`disconnect` undoes both ends; the peer's receiver is left alone if another
sender has taken it over meanwhile, and the sender goes even if the peer
can't be reached.

Every connection gets a receiver of its own: a node always has one free
receiver more than it has connections, up to `[receiver]
max` (see [several streams](#several-streams)). A node connects to the same
peer only once per priority; a second `connect` fails with `409 already
connected to … as route ID`, while one with another `--priority` is
allowed.

`PEER` is one of:

```text
Kitchen                  a name or alias, as `orpheusctl nodes` lists it
TAPXPNPX                 a node ID
kitchen.local            a host name or address; its API on port 7480
192.168.1.20
http://192.168.1.20:7481 the URL of the node's API
```

Only when all of the peer's receivers are taken (it has `max` connections)
does `connect` fail with `409 … is playing another sender`; `--force` then
takes one over. The receiver's playback stream is named after the sending
node, or its [alias](#aliases-and-pins) there, so mixers show where the
audio comes from.

A connected sender follows its peer's receiver. Each node announces a
change counter for its receivers over mDNS; when the peer's changes, or
every 30 seconds anyway, the sender asks the receiver again, so changes
arrive within a few seconds:

- The sink shows the peer's [output type](#output-types).
- The sink is renamed when the peer is, or gets an alias, unless `--name`
  named it.
- The sender follows the peer by its node ID: when the peer's address
  changes, the sender sends to the new one and tells the receiver its new
  source. When this node's own address changes, it tells the receiver too.
- If another sender takes the receiver, the sender pauses and `status`
  shows it as `waiting for a free receiver`. Every 5 seconds it looks for a
  free receiver of the peer, and takes the first one to resume. It also
  resumes when its old receiver is connected to it again, for example over
  IS-05; `disconnect` removes it.
- If a controller disconnects the receiver without connecting another
  sender, that was meant: the sender stays paused (`disconnected, paused`)
  until it is connected again or removed. A route that [claims on
  play](#claiming-a-receiver-on-play) goes `idle` instead, and claims a
  receiver again the next time audio plays.
- While the peer is offline or doesn't answer, `status` shows the sender as
  `unreachable`, after a minute `peer offline`. It keeps sending, so the
  audio is back when the peer is. The sink shows that the peer can't play
  the stream, and goes once nothing plays into it; see [lost
  outputs](#lost-outputs). It comes back with the peer, under the same
  name; if it was the default output, it is the default again.
- When the peer's [policy](#several-streams) mutes or ducks the stream,
  `status` shows `muted by policy` or `ducked`. The sender keeps sending,
  so the peer can bring the stream back at once.

The receiving end checks too: every 30 seconds each connected receiver asks
the sending node whether its sender still exists and still sends to it.
If the sender is gone, for example removed while this node was down, or
sends to another receiver now, the receiver is freed.

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

### Routes

A route is what `connect` makes: a sink on this node, streamed to a
receiver of the peer. Its ID, 8 characters like a node ID (`K7QM3XRA`),
stays the same as long as the node remembers the route, and everything
that should be the same next time hangs on it:

- the sink's machine name, `orpheus.send.<ID>`, so that applications the
  desktop's volume control moved there find it again, as does the
  [output type](#output-types) set for it;
- the sink's volume, which the node keeps with the route in `routes.json`
  (WirePlumber keeps nothing for these sinks);
- the source the stream announces (`a=x-orpheus-source` in the SDP file,
  and its NMOS source ID): the peer remembers by it on which output and at
  what volume the stream plays (see [outputs and
  volume](#outputs-and-volume)).

`disconnect` stops a route but remembers it. The next `connect` to the
same peer with the same priority brings it back with its ID and its name,
so the stream plays on the peer exactly as before. `forget` drops it.
Routes are kept in `routes.json` in the [state
directory](configuration.md#files); `orpheusd` starts the ones that ran
again when it starts. How long disconnected routes are remembered is set
in [`[routes]`](configuration.md#routes).

### Claiming a receiver on play

By default a route holds its receiver on the peer from `connect` to
`disconnect`, also while nothing plays, so that short sounds such as
alerts are heard in full. With [`[sender] claim =
"on_play"`](configuration.md#sender), it holds one only while something
plays into its sink, and the peer's receivers are free for other nodes
the rest of the time:

- When an application starts playing into the sink, sound arrives, or the
  sink becomes the default output, the route claims a free receiver of the
  peer and starts sending. That takes a few milliseconds plus the
  receiver's start, so the first moments of a sound after a pause are
  lost. Sending fades in over `fade_in_ms`, so that sound cut in midway
  doesn't click.
- After `release_after_s` (10 minutes) without sound, the route lets the
  receiver go, and `routes` and `status` show it as `idle`. The sink stays.
  An application that keeps its stream open but plays only silence does
  not claim the receiver again.
- If the peer has no free receiver when audio starts, the route waits
  (`waiting for a free receiver`, and the warning `peer_busy`) and tries
  again every 5 seconds while audio plays.
- If none of the peer's receivers can play the stream (their
  capabilities don't fit), the route waits too (`peer can't play this
  stream`, warning `peer_incompatible`), and tries again when the peer's
  receivers change, or every minute for a peer that discovery doesn't see.

Whichever receiver it gets, the peer plays the stream where it played
before and as loud, since it remembers both per
[source](#outputs-and-volume).

## Several streams

A node can receive several streams at once, one per receiver:

- **One receiver per connection.** A node offers one free receiver more
  than it has connections, up to `[receiver] max` (8), so a sender never
  has to take another's receiver. Receiver *k* listens on UDP port 5004 +
  2*k*, 5004 to 5018 with the defaults; open them all in the
  [firewall](troubleshooting.md#ptp-runs-but-never-settles). A connection
  keeps its receiver across restarts. Free receivers beyond the first go
  away after a minute.
- **One playback stream each.** A receiver runs a playback stream only
  while it is connected (with [`accept = "any"`](configuration.md#receiver)
  the free one runs too). The stream is named after the sending node, and
  plays on the output and at the volume [remembered](#outputs-and-volume)
  for its source. PipeWire mixes the streams.

Which of the streams are heard is the receiving node's
[policy](configuration.md#receiver), `[receiver] policy`:

| Policy | Heard |
|---|---|
| `last` (default) | The stream that started playing most recently; the others are muted. When it stops, the one before comes back. |
| `first` | The stream that started playing first, until it stops. |
| `mix` | All of them. |

A stream counts as playing while it carries sound above -60 dBFS, and for
5 seconds after, so a pause between two songs doesn't hand the speaker to
another stream, but a paused phone doesn't hold it either. A silent
stream is set up as it would be if it started now, so with `last` it is
heard from its first sound on. The policy only sets volumes, faded over
30 ms; it never refuses or drops a connection.

**Ducking.** A connection made with `connect --priority high` (for a
doorbell or announcements) is always heard, whatever the policy. While a
high-priority stream plays, the other streams that are heard are turned
down by `[receiver] duck_db` (20 dB). The priority travels in the sender's
SDP file, so it takes effect on any Orpheus receiver.

`orpheusctl status` shows on the receiving node which streams are `muted`
or `ducked`, and on the sending node `muted by policy` or `ducked` after
the peer.

### Outputs and volume

A receiving node remembers, for each source it played, the output the
stream plays on and its volume. A source is what one sender sends: for
another Orpheus node one of its [routes](#routes), recognized by the
source ID in the stream's SDP file; for other senders their SDP session,
else their address. Each source gets an ID of 8 characters, like a node.

- **Choosing:** move the stream in `pavucontrol` or with `wpctl`, or use
  `orpheusctl play RECEIVER OUTPUT`; change its volume in any mixer. The
  node notices within a second and keeps it in `sources.json` in the
  [state directory](configuration.md#files).
- **Playing:** when a source plays again, its stream starts on the
  remembered output at the remembered volume. If that output is missing,
  the stream waits for it; see [lost outputs](#lost-outputs). Without one
  remembered it plays on `[receiver] default_output`, else on the audio
  system's default output, and follows that default when you change it.
- **Moving:** `orpheusctl play` puts the stream on the output in place of
  any [wires](#wires) of its source, in one step.
- **Not passed on:** a stream from another node is never played into a
  sink to another node unless you choose that. If the only output left is
  such a sink (the default output is one, as on a desktop that sends its
  audio elsewhere), the stream waits unlinked: `orpheusctl status` shows
  the receiver's output as `none` and the node warns (`held`). Choose an
  output with `orpheusctl play`, make a local output the default with
  `orpheusctl default`, or set `default_output`.
- **Forgetting:** `orpheusctl forget-source` drops a source's output,
  volume and [layout](#channel-layouts). Unused sources are forgotten after the [`[sources]`
  limits](configuration.md#sources); those from pinned nodes are kept.

```sh
orpheusctl sources
orpheusctl play 0 "Built-in Audio"
orpheusctl default "Built-in Audio"
orpheusctl set receiver.default_output=alsa_output.pci-0000_00_1f.3.analog-stereo
orpheusctl forget-source Desk
```

WirePlumber restores nothing for these streams: they carry
`state.restore-props` and `state.restore-target` set to `false`.

### Lost outputs

A stream never moves to another room by itself. When the output it plays
on goes away (a speaker switched off, a USB or Bluetooth device gone, the
default output removed), the stream waits there, unlinked, and plays
again as soon as the output is back:

- **Hiccups:** an output back within `[receiver] lost_after_ms` (2 s) is
  not lost; the stream just carries on.
- **Lost:** after that the stream is parked. `orpheusctl status` shows
  `parked: Kitchen speaker is gone`, the API the receiver's `parked`.
  Play it elsewhere with `orpheusctl play`, or bring the output back.
- **Last wire removed:** removing a source's last [wire](#wires) is a
  stop, not a move back to its output: the source is parked until played
  with `orpheusctl play` or wired again, also when it starts again later.
- **On the sending node:** while the peer can't play the stream (its
  output lost, its last wire removed, or the peer unreachable for
  `lost_after_ms`), the route's sink is called `TV (unavailable)` (in
  the [language](configuration.md#ui) set), and
  `status` shows `its output there is gone`, `unwired there` or
  `unreachable`. The sink stays while an application plays into it, so
  that the application moves nowhere else; a paused one counts too. Once
  none does, the sink is hidden (`sink hidden`) until the peer can play
  the stream again. Sinks to [pinned](#aliases-and-pins) nodes stay.
- **`when_lost`:** `pause` (the default) is meant to pause the player on
  the sending node; not yet, for now it parks. `park` parks. `default`
  plays a lost stream where streams without an output play, after
  `lost_after_ms`, and moves it back when its output returns; removing
  the last wire then plays the source on its output again. On the
  sending node, `default` hides a route's sink once the peer has been
  offline for a minute, whatever plays into it, so that applications
  move on to another output.

```sh
orpheusctl set receiver.when_lost=default
orpheusctl set receiver.lost_after_ms=5000
```

GNOME's sound settings may go on showing a stream as playing on the
output it lost, and show a sink by the name it had when it appeared,
without `(unavailable)`; `orpheusctl status` is right.

**Two volumes for one route.** The sink on the sending node ("Kitchen")
and the stream on the receiving node both scale the same audio, and they
multiply: 50% on each (as mixers show it) is 25%. The sink's volume is
applied before sending, so it changes what goes over the network; the
stream's volume is applied where it plays, before the output's own
volume. Neither costs quality: audio stays floating point in PipeWire and
24 bit on the wire. Use the sink's for "this computer to the kitchen",
the stream's for "this source in this room". Each is remembered by its
own node.

### Channel layouts

Each channel of a stream says what it carries, by a channel symbol from
NMOS: `L`, `R`, `C`, `LFE`, `Ls`, `Rs` and the like, `M1` and `M2` for
mono, `NSC001` for numbered and `U01` for undefined channels, each with a
label to show, such as "Left" or "Mic 1". A sender announces them for its
source over NMOS (IS-04), and as the SMPTE ST 2110-30 channel grouping in
its SDP file, `a=fmtp:97 channel-order=SMPTE2110.(ST)`. The grouping knows
mono (`M`), dual mono (`DM`), stereo (`ST`), matrix stereo (`LtRt`), 5.1
(`51`), 7.1 (`71`), 22.2 (`222`), SDI groups (`SGRP`) and undefined runs
(`U01` to `U64`); channels it has no group for go as undefined.

A receiver takes the layout from the stream's SDP file. If the file says
nothing, as with many AES67 devices, it assumes one from the channel
count: mono, stereo, 5.1 or 7.1, else undefined channels. `orpheusctl
status` shows each channel as "label (symbol)", and the playback stream
places the channels accordingly: `L` and `R` on the front left and right,
undefined ones as numbered AUX channels, which PipeWire passes on in
order.

Where the output has other channels than the stream, PipeWire mixes the
stream onto them by position: 5.1 onto stereo is a downmix, stereo onto
5.1 an upmix (PipeWire's own, which makes centre and rear channels up),
and as many channels but other ones, such as dual mono onto stereo, a
remix. `status` says so, so that no mix goes unnoticed:

```
Receiver 0     ● receiving from Laptop
  channels     Left (L), Right (R)
  output       speaker (Kitchen 5.1)
  mix          upmix ST → 51
```

For a sender that says nothing or says it wrong, set the layout of its
[source](#outputs-and-volume); a stream playing it restarts with it, and
it is kept with the source:

```sh
orpheusctl layout Desk DM       # two mono channels
orpheusctl layout Desk R L      # sent the wrong way round
orpheusctl layout Desk auto     # what the stream says again
```

Over the API, a layout can carry labels:

```sh
curl -X PUT localhost:7480/x-orpheus/v1/sources/<id>/layout -H 'content-type: application/json' \
  -d '{"layout": [{"symbol": "U01", "label": "Mic 1"}, {"symbol": "U02", "label": "Mic 2"}]}'
```

Streams have two channels for now; a layout with another number is
refused. Orpheus' own senders send stereo, `L` and `R`.

### Inputs and outputs

`orpheusctl outputs` lists what can be played on: the sound cards and
other sinks, the sinks to other nodes, and outputs created from channels
of these. `orpheusctl inputs` lists what can be played: each running
receiver's stream (`orpheus.receive.<id>`) and the audio system's inputs,
such as microphones and line-ins. Both show the channels as a
[layout](#channel-layouts): `ST`, `M`, `51`, or `U08` for eight numbered
channels, as audio interfaces in their "pro audio" profile have.

An audio interface with several outputs shows up as one output with many
channels. To play into some of them as into an output of their own,
create one:

```sh
orpheusctl output create Kitchen "Scarlett 4i4" 3 4   # its channels 3 and 4
orpheusctl output create Desk "Built-in Audio" 2 --layout M
orpheusctl output remove Kitchen
```

The new output (`orpheus.output.kitchen`) is a PipeWire sink like any
other: applications and receivers play into it, it has a volume, a
[type](#output-types), and can be the default. Its channels are linked to
the chosen channels of the device, one each, in order; by default it has
the layout their number suggests (`ST` for two, `M` for one). It is kept
in `outputs.json` and created again at start. While the device is
missing, `outputs` marks it `(missing)` and what is played into it goes
nowhere; once the device is back, it plays there again.

Inputs are played on outputs with [wires](#wires).

### Wires

A wire plays channels of a source, or of an input such as a microphone,
on chosen channels of an output: a device, a created output, or a sink to
another node. Each pair names an output channel and the input channel
played on it, `OUT=IN`, by number (from 1), [symbol](#channel-layouts) or
label:

```sh
orpheusctl wire TV Kitchen                  # all channels, where they belong
orpheusctl wire TV Kitchen L=R R=L          # left and right swapped
orpheusctl wire TV "Scarlett 4i4" 3=L 4=R   # on channels 3 and 4
orpheusctl wire "USB mic" Kitchen 1=1 2=1   # a mono mic on both sides
orpheusctl unwire TV Kitchen 3=L            # one pair
orpheusctl unwire TV Kitchen                # the whole wire
orpheusctl patch
```

`FROM` is a [source](#outputs-and-volume) by name or ID, as `sources`
lists them, or an input by name or description, as `inputs` lists them.
A source is wired, not the receiver playing it: the wire applies to
whichever receiver plays the source, and waits while none does. A source
with wires plays only on them, not on its output; without wires it goes
back there.

Without pairs, each channel goes to the output channel of the same
symbol. What the output lacks is folded onto it: side and rear channels
onto its left and right, the centre onto its centre or else both sides,
mono onto every channel; the LFE is left out. Channels with no symbols in
common go by number. Folded channels are summed as they are, not turned
down, and can clip.

`wire` and `patch` mark each channel that plays on another one as part of
a mix with `⤵`, and say what the wire does: a downmix where it sums
channels onto one or leaves some out (`downmix 51 → ST, LFE left out`), an
upmix where it spreads one onto several. Stereo onto 5.1 plays on the
front left and right only, which is no mix.

What several wires bring into one output channel is summed, too.
`patch` shows it as it is, every input on each output channel:

```
Kitchen (orpheus.output.kitchen)
  L  ← TV: R, USB mic: M1⤵
  R  ← TV: L, USB mic: M1⤵
  ⤵ USB mic: upmix M → ST

Waiting
  Radio → Kitchen (all): Radio plays nowhere
```

Wires are kept in `wires.json` in the [state
directory](configuration.md#files). A wire waits while its source plays
nowhere or an end is missing, and links again once it is back. On Linux,
each pair is a PipeWire link from the input's port to the output's port;
`pw-link -l` shows them.

## Aliases and pins

An **alias** is this node's own name for another node. It replaces the
announced name wherever this node shows the peer: in `orpheusctl nodes`,
in the name of a sink to it (unless `connect --name` named the sink), and
in the name of a stream received from it. `connect` finds the peer by its
alias too. Aliases are unique on a node, ignoring case.

A **pinned** node stays listed in `orpheusctl nodes` while it is offline,
also after a restart, as it was last seen.

```sh
orpheusctl alias Küche Kitchen         # by name, alias or ID
orpheusctl connect Kitchen
orpheusctl pin Kitchen
orpheusctl nodes
orpheusctl alias Kitchen --remove
orpheusctl unpin TAPXPNPX
```

Both are kept by node ID in `peers.json` in the [state
directory](configuration.md#files), so they survive a peer's change of name
or address. They are local: other nodes don't see them.

## Reading the status

```text
desk  B73AQ0P0  orpheusd 0.0.1

Warnings
  ● route Kitchen: Kitchen is on Wi-Fi; expect dropouts

Clock          ● PTP, settled
  interface    enp3s0 (IPv4)
  identity     4cb04a.fffe.50a582
  state        slave
  grandmaster  9c6b00.fffe.1a2b3c
  offset       -0.4 µs
  delay        41.2 µs
  frequency    +12.31 ppm
  steps        1

Receiver 0     ● receiving from Kitchen
  listen       [::]:5004
  output       hifi (Built-in Audio Analog Stereo)
  link offset  10 ms
  level        -12.0 dBFS
  margin       8.71 ms
  jitter       0.21 ms
  packets      120000
  buffer       1140 frames
  ASRC         -3.2 ppm
  problems     none

Receiver 1     ○ free, port 5006

Senders
  ID  NAME     PEER                                 PACKETS       BUFFER      ASRC  PROBLEMS
  0   Kitchen  192.168.1.20:5004                     120000  1120 frames  +8.9 ppm  none
  1   Bath     192.168.1.30:5006 (muted by policy)    80000  1124 frames  -2.1 ppm  none
```

On a terminal the dots are coloured: green is fine, yellow needs attention
(settling, on the system clock), red is broken. Problem counters, a
`margin` below 1 ms and more than two `steps` are highlighted too. Colour is
off when the output is piped or `NO_COLOR` is set.

Counters are totals since the start. With `--watch` they are rates over the
last interval instead, so `problems` shows only what is going wrong now.

**Warnings** are listed only when there are any, and logged as they come
and go; see [warnings](#warnings).

**Clock**

- `PTP, settled` or `PTP, settling`: see [clock](clock.md#ptp).
  `PTP not running, system clock` means PTP could not start yet, and
  `PTP stopped, holding over` that it stopped after following a
  grandmaster; `error` says why, and the node tries again by itself (see
  [clock](clock.md#when-ptp-cant-run)). `system clock` alone means PTP is
  off (`[ptp] enabled = false`).
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

**Receivers**, one per connection and a free one (see [several
streams](#several-streams)):

- `receiving from …`: the connected sender, by its node's name or
  [alias](#aliases-and-pins), and audio arrives; `connected to …, idle` if
  nothing arrives. `, muted` or `, ducked` means the
  [policy](#several-streams) holds the stream back.
  `receiving ssrc …` is an unconnected receiver that accepts any stream.
  `free, port …` means no sender is connected and the receiver plays
  nothing; `free, plays any sender` that it would play any stream sent to
  its port but none arrives; `connected to …, off` that a
  [connection](#connections-is-05) switched it off.
- `channels`: what each channel carries, as "label (symbol)"; see
  [channel layouts](#channel-layouts).
- `output`: the kind of output it plays on, which other nodes show, and
  that output (see [output types](#output-types)).
- `priority`: `high` for a connection made with `--priority high`; shown
  only then.
- `level`: the stream's peak level over the last second, before the
  policy's volume, or `silence`.
- `ignored`: packets turned away because they are not from the connected
  sender, or the receiver plays nothing. Shown only when there are any.
- `margin`: the smallest time between a packet's arrival and its playout over
  the last second. It should stay well above zero. If it gets close to zero,
  raise `link_offset_ms`.
- `jitter`: how far the packets' transit times spread over the last
  second. A few hundred µs is normal on a wired network; above 3 ms it is
  highlighted and the node [warns](#warnings).
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
there is no route to the peer, or because the link stalled and its send
queue was full (Wi-Fi). After the peer, the `PEER` column may show
the sender's state:

- `(off)`: a connection paused the sender. Its sink still takes audio, but
  nothing is sent.
- `(disconnected, paused)`: a controller disconnected the peer's receiver,
  so nothing is sent until it is connected again (see
  [connecting](#connecting)).
- `(waiting for a free receiver)`: another sender took the peer's
  receiver, or all were taken when audio started; nothing is sent until
  the peer has a free one.
- `(peer can't play this stream)`: none of the peer's receivers can play
  the stream; nothing is sent until one can.
- `(idle)`: the route [claims a receiver on
  play](#claiming-a-receiver-on-play) and nothing has played for a while;
  nothing is sent until audio plays.
- `(unreachable)`: the peer is offline or doesn't answer. The sender keeps
  sending.
- `(peer offline)`: the peer has been unreachable for a minute.
- `(its output there is gone)`, `(unwired there)`: the peer plays the
  stream nowhere (see [lost outputs](#lost-outputs)). The sender keeps
  sending.
- `(sink hidden)`: the peer can't play the stream and nothing played into
  the sink, so it is gone until the peer can again.
- `(connecting)`: `connect` is still setting up the peer's receiver.
- `(muted by policy)`, `(ducked)`: the peer's [policy](#several-streams)
  holds the stream back. The sender keeps sending.
- `(high priority)`: made with `connect --priority high`.

**Buffers** (receiver, senders, loopback)

- `buffer`: fill level of the ring between PipeWire and the network thread.
  In both directions it is about one block plus a few milliseconds:
  PipeWire moves a whole block per cycle, the network threads 1 ms (see
  [`[audio]`](configuration.md#audio)).
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
| `GET /x-orpheus/v1/status` | Node status as JSON (what `orpheusctl status` shows); see [below](#the-status-json). |
| `GET /x-orpheus/v1/routes` | The [routes](#routes) that run, then the remembered ones, most recently used first: `id`, `name`, `priority`, `peer` (address), `node` (the peer's node ID), `active`, for running routes to a peer's receiver `state` (as in [the status JSON](#the-status-json)), and for remembered ones `last_used` (Unix seconds). |
| `POST /x-orpheus/v1/routes` | Starts a route and [connects](#connecting) a receiver of the peer to it. Body: `{"peer": "Kitchen"}`, optionally with `"name"`, `"priority": "high"`, `"force": true` or `"send_only": true`. Returns `201` with `{"id": "K7QM3XRA", "name": "Kitchen", "destination": "192.168.1.20:5004", "receiver": "<uuid>"}`; `409` if the peer has no free receiver or this node is already connected to it with the same priority, `422` if there is no such peer or none of its receivers can play the stream, `502` if it can't be reached or refuses. |
| `DELETE /x-orpheus/v1/routes/{id}` | Stops a route and disconnects the peer's receiver; the route is remembered. With `?forget=true` it is forgotten, also one that doesn't run. Returns `204`, or `404` if there is no such route. |
| `GET /x-orpheus/v1/routes/{id}/sdp` | A running route's stream as an SDP file (`application/sdp`). |
| `GET /x-orpheus/v1/sources` | The [sources](#outputs-and-volume) the receivers remember, most recently played first: `id`, `name`, `key` (what the stream is recognized by), `node` (the sending node's ID, if Orpheus), `output` (machine name, absent for the default), `volume` (`channels`, linear per channel, and `mute`), `layout` (if [set](#channel-layouts): `symbol` and `label` per channel), `receiver` (while it plays) and `last_used` (Unix seconds). |
| `DELETE /x-orpheus/v1/sources/{id}` | Forgets a source's output, volume and layout; a stream playing it goes to the default output at full volume. Returns `204`, or `404` if there is no such source. |
| `PUT /x-orpheus/v1/sources/{id}/layout` | Sets a source's [layout](#channel-layouts): `{"layout": "51"}` (ST 2110-30 groups or channel symbols) or `{"layout": [{"symbol": "L", "label": "Left"}, …]}`. A stream playing it restarts. Returns `204`; `400` for a malformed layout, `404` if there is no such source, `422` for another number of channels than the streams have. |
| `DELETE /x-orpheus/v1/sources/{id}/layout` | Goes back to what the source's stream says. Returns `204`, or `404`. |
| `PUT /x-orpheus/v1/receivers/{id}/output` | Plays a receiver's stream on an output, remembered for its source. Body: `{"output": "<name>"}`. Returns `204`; `404` if there is no such receiver or output, `409` if the receiver is not playing. `DELETE` goes back to the default. |
| `GET /x-orpheus/v1/outputs` | The outputs: `name`, `description`, `type`, `detected`, `override` (if set), `default`, `receivers` (the IDs of the receivers that play there), `channels` (`symbol` per channel, see [channel layouts](#channel-layouts)) and, for a [created output](#inputs-and-outputs), `created`: the `device` it plays on and that device's `channels`, counted from 1. |
| `POST /x-orpheus/v1/outputs` | Creates an output on channels of another. Body: `{"name": "Kitchen", "device": "<name>", "channels": [3, 4]}`, optionally with `layout` as for [sources](#channel-layouts). Returns `201` with its `name`; `400` for a malformed name or layout, `409` if an output of that name exists, `422` if there is no such device or not those channels, `503` without an audio system. |
| `DELETE /x-orpheus/v1/outputs/{name}` | Removes a created output. Returns `204`, or `404` if there is no such created output. |
| `PUT /x-orpheus/v1/outputs/{name}/type` | Sets an output's type. Body: `{"type": "hifi"}`. Returns `204`, or `404` if there is no such output. |
| `DELETE /x-orpheus/v1/outputs/{name}/type` | Goes back to the detected type. Returns `204`, or `404` if no type was set. |
| `GET /x-orpheus/v1/inputs` | The [inputs](#inputs-and-outputs): `name`, `description`, `kind` (`receiver` or `device`), `receiver` (its ID, for a receiver) and `channels`. |
| `GET /x-orpheus/v1/wires` | The [wires](#wires): `source` (ID) or `input` (machine name), `from_name`, `output`, `output_name`, `channels` (the pairs as set, absent for all), `links` (what it links now: `from` and `to` channel, counted from 1, whether the audio system has made the link, and `folded` for a channel played on another one as part of a mix), `mix` (`downmix` or `upmix`, absent where each channel plays on its own), `dropped` (the channels a wire of all channels leaves out, such as the LFE) and, while known, `from_channels` and `output_channels` as [layouts](#channel-layouts). |
| `PUT /x-orpheus/v1/wires` | Sets the wire between a source or input and an output, replacing one there. Body: `{"source": "<id>", "output": "<name>"}` or `{"input": "<name>", …}`, optionally with `"channels": [{"to": "L", "from": "R"}, {"to": 3, "from": 1}]`. Returns `200` with the wire as listed; `400` for a malformed request or no pairs, `422` if there is no such source, input or output, or no such channel, `503` without an audio system. |
| `DELETE /x-orpheus/v1/wires?source=<id>&output=<name>` | Removes a wire; `input=<name>` instead of `source` for an input. Returns `204`; `400` without either, `404` if there is no such wire. |
| `PUT /x-orpheus/v1/outputs/{name}/default` | Makes the output the default one. Returns `204`, `404` if there is no such output, `500` if the audio system doesn't change it, or `503` without an audio system. |
| `GET /x-orpheus/v1/nodes` | The other nodes found over mDNS (what `orpheusctl nodes` shows), online first, then by the name shown, and pinned nodes that are offline. Each has `id`, `name`, `alias` (if set), `pinned`, `online`, `version`, `host`, `addresses`, `port` and `link`. Returns `503` if mDNS could not start. |
| `PATCH /x-orpheus/v1/nodes/{id}` | Sets a node's [alias or pin](#aliases-and-pins), by its node ID. Body: `{"alias": "Kitchen"}`, `{"alias": null}` to remove it, `{"pinned": true}`, or both keys. Returns `204`; `400` for an empty alias, `404` if the node was never seen, `409` if another node has the alias. |
| `GET /x-orpheus/v1/settings` | The [settings](configuration.md#settings-changed-over-the-api) in effect, by section (`ptp`, `receiver`, `sender`), and `sources`: for each key where its value comes from, `default`, `config` or `api`. |
| `PATCH /x-orpheus/v1/settings` | Changes settings, as a JSON merge patch: `{"receiver": {"policy": "mix"}}` sets a value, `null` in its place resets it. Returns `200` with the settings now in effect; `400` for an unknown key or a bad value, `422` if PTP can't start with the new values. Nothing changes on an error. |
| `GET /x-orpheus/v1/events` | A WebSocket that pushes the node's state, see [below](#events). |
| `GET /x-orpheus/v1/openapi.json` | The OpenAPI 3.1 description of the API. |

The same OpenAPI document is printed by `orpheusd --openapi`.

Errors come as JSON, as in the NMOS APIs: `{"code": 404, "error": "no route
3XJ7Q2WC", "debug": null}`. The status codes mean the same on every
endpoint:

| Code | Meaning |
|---|---|
| `400` | The request is malformed: not JSON, a field missing or of the wrong type, an invalid value. |
| `404` | The path, or the route, source, output, receiver or node it names, doesn't exist. |
| `409` | The state of this node or the peer is in the way; it may work later or with `force`: the peer's receivers are busy, the receiver isn't playing, the alias is taken. |
| `422` | The request is fine, but it can't work: the peer is unknown, none of its receivers can play the stream, PTP can't run with the new settings. |
| `500` | The node failed, for example the audio system didn't do what it was asked. |
| `502` | The peer can't be reached, or answered with an error. |
| `503` | A part of the node isn't running: the audio system or mDNS. |

The NMOS APIs also use `423` (IS-05: an activation is scheduled) and `501`
(IS-04's deprecated receiver `target`).

### Events

Instead of polling, a client can open a WebSocket on
`/x-orpheus/v1/events` and is sent each topic's full state, as the GET of
the same name returns it, when it connects and again whenever it changes:

```json
{"topic": "routes", "data": [{"id": "K7QM3XRA", "name": "Kitchen", "…": "…"}]}
```

| Topic | As |
|---|---|
| `status` | `GET /x-orpheus/v1/status`, at most once a second |
| `nodes` | `GET /x-orpheus/v1/nodes`; `null` while mDNS isn't running |
| `outputs` | `GET /x-orpheus/v1/outputs`, at most once a second |
| `settings` | `GET /x-orpheus/v1/settings` |
| `routes` | `GET /x-orpheus/v1/routes` |
| `sources` | `GET /x-orpheus/v1/sources`, at most once a second |
| `inputs` | `GET /x-orpheus/v1/inputs`, at most once a second |
| `wires` | `GET /x-orpheus/v1/wires`, at most once a second |

`?topics=status,routes` picks topics; an unknown one is a `400`. The
others come within 250 ms of a change. A slow client skips states but
never gets old ones. Messages from the client are ignored.
`orpheusctl status --watch` uses this socket.

```sh
websocat 'ws://localhost:7480/x-orpheus/v1/events?topics=settings'
```

### The status JSON

```json
{
  "id": "B73AQ0P0", "name": "desk", "version": "0.0.1", "audio": true,
  "clock": {"source": "ptp", "ptp": {"offset_ns": -412, "settled": true, "…": "…"}},
  "accept": "connected", "policy": "last", "duck_db": 20.0,
  "receivers": [
    {"id": 0, "uuid": "…", "port": 5004, "sender": "Kitchen",
     "state": "playing", "priority": "normal",
     "output": "hifi", "plays_on": "Built-in Audio Analog Stereo",
     "stream": {"listen": "[::]:5004", "packets": 120000, "peak_dbfs": -12.0, "…": "…"}},
    {"id": 1, "uuid": "…", "port": 5006, "priority": "normal", "output": "hifi"}
  ],
  "routes": [
    {"id": "5c1e0b6a-…", "name": "Kitchen", "peer": "192.168.1.20:5004", "priority": "normal",
     "packets": 120000, "…": "…",
     "receiver": {"api": "http://192.168.1.20:7480", "id": "…",
                  "state": "connected", "policy": "muted"}}
  ],
  "warnings": []
}
```

- `clock.source`: `ptp` while PTP runs, `holdover` while it is stopped
  after following a grandmaster, `system` otherwise. `clock.ptp` is there
  whenever PTP is enabled, also while it isn't running: `running`, and if
  not, `error` (why) and `holdover`.
- `accept`, `policy`, `duck_db`: the [`[receiver]`
  settings](configuration.md#receiver).
- `receivers`: one per connection and the free one. `sender` names the
  connected sender (its node's alias or name, else its address); `stream`
  is there while the receiver runs; `layout` is what the
  stream's channels carry, `symbol` and `label` per channel (see
  [channel layouts](#channel-layouts)); `state` is what the
  [policy](#several-streams) does to it: `silent`, `playing`, `muted` or
  `ducked`. `plays_on` names the output it plays on and
  `output_channels` its channels; `mix` is how the audio system mixes the
  stream onto them, `downmix`, `upmix` or `remix`, absent where they
  match (see [channel layouts](#channel-layouts)). `held` says it plays
  nowhere (see [warnings](#warnings)), `wired` that it plays on
  [wires](#wires).
- `routes`: the routes that run. `playing` says whether applications play
  into the sink, `silent_ms` how long ago it last carried sound, and
  `hidden` whether the sink is gone while the peer can't play the
  stream.
  `routes[].receiver`: the peer's receiver, absent with `send_only`.
  `state` is `connecting`, `connected`, `disconnected`, `unreachable`,
  `idle` (let go after silence, see [claiming on
  play](#claiming-a-receiver-on-play)), `busy` (waiting for a free
  receiver), `incompatible` (no receiver of the peer can play the stream)
  or `offline` (unreachable for a minute);
  `policy` is `muted` or `ducked` while the peer holds the stream back;
  `parked` is `lost` or `unwired` while the peer plays it nowhere (see
  [lost outputs](#lost-outputs)).
- `warnings`: a list of `{kind, on, sender, message}`, see
  [warnings](#warnings).

```sh
curl -s localhost:7480/x-orpheus/v1/status | jq .clock
curl -s -X POST localhost:7480/x-orpheus/v1/routes \
  -H 'content-type: application/json' -d '{"peer": "Kitchen"}'
```

## Warnings

A node warns about network paths that put the audio at risk. Each warning
is shown by `orpheusctl status`, in the status JSON, and logged when it
appears (`WARN`) and when it is resolved (`INFO`):

| `kind` | On | Meaning |
|---|---|---|
| `wifi` | clock, receiver, sender | The stream leaves this node over Wi-Fi, or the clock follows its grandmaster over Wi-Fi. Wi-Fi delays packets by milliseconds and holds back multicast, so expect dropouts and clocks that don't settle. |
| `wifi_grandmaster` | clock | PTP runs over Wi-Fi on a node that does not follow another clock, so it is or may become the grandmaster. That does not hurt this node, but nodes that follow its clock may not settle. Give a wired node a lower `ptp.priority1` so that it becomes the grandmaster. |
| `multicast_wifi` | receiver, sender | A [multicast](#multicast-interop) stream goes over Wi-Fi, where the access point sends it at its lowest rate to every client. |
| `peer_wifi` | receiver, sender | The node at the other end is on Wi-Fi, as it announces. |
| `jitter` | receiver | The packets' transit times spread by more than 3 ms in 3 of the last 10 seconds: Wi-Fi or a congested link somewhere on the way, also where neither node is on Wi-Fi itself (a wireless bridge, a mesh). |
| `peer_jitter` | sender | The peer's receiver warns about `jitter` for this sender. |
| `ptp_delay` | clock | The path to the PTP grandmaster takes more than 0.5 ms; wired it takes tens to a few hundred µs. |
| `ptp_down` | clock | PTP is not running, so the node plays out of step with the other nodes: on the system clock, or holding over the grandmaster's last time and rate. It tries again by itself; see [clock](clock.md#when-ptp-cant-run). |
| `ptp_steps` | clock | The PTP clock stepped 3 times within a minute instead of settling, which a slow or unsteady path to the grandmaster causes. |
| `held` | receiver | The stream plays nowhere: the default output is a sink to another node, and streams are passed on to other nodes only when chosen. See [outputs and volume](#outputs-and-volume). |
| `peer_busy` | sender | The route wants a receiver on the peer, but all of the peer's receivers play other senders; it tries again every 5 seconds. Raise `[receiver] max` on the peer, or let other routes [claim on play](#claiming-a-receiver-on-play). |
| `peer_incompatible` | sender | None of the peer's receivers can play the route's stream; it tries again when the peer's receivers change. |

Wi-Fi is recognized on Linux by the interface (`/sys/class/net/<if>/wireless`)
the stream or PTP takes. Each node also announces its own link, that of its
default route, in its mDNS record (`x-orpheus-link`), so the node at the
other end warns too, and `orpheusctl nodes` shows it. The receiver tells its
senders about `jitter` through its NMOS resource (the tag
`urn:x-orpheus:warnings`), which the senders [follow](#connecting).

## Output types

Every output has a type: `speaker`, `headphones`, `headset`, `tv`, `hifi`,
`computer`, `portable`, `phone` or `generic`. A node announces, for each
receiver, the type of the output it plays on, and nodes connected to it
show their sink to it with a matching icon and PipeWire form factor.

- **Which output:** the one the receiver's stream is linked to in PipeWire,
  so moving the stream in `pavucontrol` changes the announced type; while
  it is not linked, or the receiver is free, the default output.
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

- a **receiver** per connection and a free one (see [several
  streams](#several-streams)), up to 8 by default. Its capabilities
  (`caps`, after BCP-004-01) say what it plays: L24, 48 kHz, 2 channels;
- per sender started with `connect`, a **source**, with its `channels`
  (symbol and label, see [channel layouts](#channel-layouts)), a **flow**
  (L24, 48 kHz, stereo) and a **sender**.

```sh
curl -s localhost:7480/x-nmos/node/v1.3/self | jq
curl -s localhost:7480/x-nmos/node/v1.3/senders | jq '.[].label'
```

Each sender's `manifest_href` points to its SDP file, which describes the
stream the way AES67 and SMPTE ST 2110-30 receivers expect: destination,
L24/48000/2, the channel grouping (`channel-order=SMPTE2110.(ST)`), payload
type 97, 1 ms packets, the PTP grandmaster and domain (`ts-refclk`) and
`mediaclk:direct=0`:

```sh
curl -s localhost:7480/x-orpheus/v1/routes/<id>/sdp
```

Without PTP the SDP names the sender's own clock (`ts-refclk:localmac=…`).
The session name (`s=`) is the node's name; receivers name their playback
stream after it. Three attributes are Orpheus' own, and other receivers
ignore them: `a=x-orpheus-node:<id>` names the sending node, so its
receiver can check back that the sender still exists,
`a=x-orpheus-source:<uuid>` names what is sent (the [route](#routes)'s
NMOS source), and
`a=x-orpheus-priority:high` marks a [high-priority](#several-streams)
connection.

Resource IDs are derived from the node ID, and a sender's from its
route, so they stay the same across restarts and reconnects. A route's
NMOS source ID is derived from the node and route IDs. Each receiver carries tags of its own:

- `urn:x-orpheus:form-factor`: the [type](#output-types) of the output it
  plays on;
- `urn:x-orpheus:state`: `muted` or `ducked` while the
  [policy](#several-streams) holds its stream back;
- `urn:x-orpheus:warnings`: `jitter`, see [warnings](#warnings).

Senders follow these tags. Nodes don't register with an NMOS registry; they
only use peer-to-peer discovery.

### Connections (IS-05)

The IS-05 Connection API v1.1 at `/x-nmos/connection/v1.1/` routes the
senders and the receivers. Each has **staged** parameters, which a controller
changes with `PATCH`, and **active** ones, which the node runs. Staged
parameters become active at once (`activate_immediate`) or at a TAI time
(`activate_scheduled_absolute`, `activate_scheduled_relative`). `auto`
values are resolved in the active parameters.

- **Senders:** `destination_ip` and `destination_port` (unicast, or a
  multicast group for [interop](#multicast-interop)), `source_ip` and
  `source_port`. `master_enable: false`
  pauses the sender. The SDP file is at `…/senders/{id}/transportfile`.
- **Receivers:** a `transport_file` (the sender's SDP) connects one to that
  sender: it then only plays packets from the sender's address with its
  payload type, and joins its multicast group if it has one. `source_ip`,
  `multicast_ip` and `destination_port` can also be set directly.
  `master_enable: false` switches it off. Without a source address or
  group it plays nothing, or, if it [accepts any
  stream](configuration.md#receiver), whatever arrives on its port.
  Connecting the free receiver makes the node offer a new free one;
  `destination_port: "auto"` is the receiver's own port.

For example, to connect this node's free receiver to a sender of another
node:

```sh
C=localhost:7480/x-nmos/connection/v1.1/single
RX=$(curl -s localhost:7480/x-orpheus/v1/status |
  jq -r '[.receivers[] | select(.sender == null)][0].uuid')
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
airtime. A node [warns](#warnings) (`multicast_wifi`) when it sends or
joins a group over Wi-Fi.

The API has no authentication yet. Restrict `[api] listen` to `127.0.0.1` if
the network is not trusted.
