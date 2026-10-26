# What orpheusctl shows. Each message is one line; Fluent would keep line
# breaks. Texts from the node, such as warnings, come in the node's
# language (its [ui] language).

## Help. `cmd-<command>` describes a command, `arg-<command>-<argument>`
## one of its arguments; `arg-<argument>` the options of all commands.

about = Command-line client for Orpheus nodes
help-usage = Usage:
help-commands = Commands
help-arguments = Arguments
help-options = Options
help-help = Print help
help-version = Print version
cmd-help = Print this message or the help of the given command
arg-help-command = The command whose help to print

arg-node = The node to talk to: a host name or address (API on port 7480) or the URL of its API; localhost if neither this nor ORPHEUS_NODE is set.
arg-json = Print the node's JSON responses instead of text.
cmd-status = Show the node's ID, name, version and streams.
arg-status-watch = Follow the node: refresh as it changes, at most every SECONDS (default 1), with counters as rates.
cmd-connect = Send the node's audio to a peer: creates a sink "<name>" whose audio is streamed to a free receiver of the peer, and connects that receiver to it. A route to the peer with the same priority from before comes back as it was.
arg-connect-peer = Peer: its name or ID (see `nodes`), host name or address, or the URL of its API. With --send-only: host name or address, optionally with :port (default 5004); IPv6 with a port as [addr]:port.
arg-connect-name = Name shown in the sink; defaults to the route's name from before, else the peer's name.
arg-connect-send_only = Only send; don't connect a receiver on the peer (for receivers without an NMOS API, or set to accept any stream).
arg-connect-force = Take over the peer's receiver even if another sender is connected to it.
arg-connect-priority = `normal` (the default) or `high`: plays over the peer's other streams and turns them down meanwhile, whatever the peer's policy (for announcements).
arg-connect-layout = What the sink's channels carry, and with them the stream's: ST 2110-30 groups (`51`, `71`, `U16`) or channel symbols (`"L R C"`); defaults to the route's layout from before, else stereo.
cmd-nodes = List the other nodes on the network.
cmd-alias = Give another node a name of your own, shown here instead of the one it announces: in `nodes`, in sinks to it and in streams from it.
arg-alias-node = The node: its name, alias or ID (see `nodes`).
arg-alias-alias = The new alias.
arg-alias-remove = Remove the alias and show the announced name again.
cmd-pin = Keep another node listed while it is offline, also after a restart.
arg-pin-node = The node: its name, alias or ID (see `nodes`).
cmd-unpin = Stop keeping a node listed while it is offline.
arg-unpin-node = The node: its name, alias or ID (see `nodes`).
cmd-outputs = List the node's outputs, their types and channels.
cmd-inputs = List the node's inputs: the receivers' streams and the audio system's inputs, such as microphones.
cmd-output = Create or remove outputs made of channels of another.
cmd-output-create = Create an output that plays on chosen channels of another, such as channels 3 and 4 of an audio interface. Applications and receivers play into it like into any output; kept across restarts.
arg-output-create-name = Its name, e.g. Kitchen.
arg-output-create-device = The output to play on: its name or description, or a unique part of either.
arg-output-create-channels = That output's channels, counted from 1, one per channel of the new output: `3 4`.
arg-output-create-layout = What the channels carry, as ST 2110-30 groups or symbols; by default stereo for two, and so on.
cmd-output-remove = Remove a created output.
arg-output-remove-output = The output: its name or description, or a unique part of either.
cmd-set-type = Set what kind of device an output is, which other nodes show.
arg-set-type-output = The output: its name or description, or a unique part of either.
arg-set-type-type = speaker, headphones, headset, tv, hifi, computer, portable, phone or generic; `auto` goes back to the detected type.
cmd-play = Play a receiver's stream on another output, in place of any wires of its source; also one that is parked. The node remembers it for what is sent, and plays it there again later.
arg-play-receiver = Receiver ID, as shown by `status`.
arg-play-output = The output: its name or description, or a unique part of either; `default` goes back to the default output.
cmd-default = Make an output the default one: applications play there, and so do received streams that have no output of their own. Another node's sink is fine too. For nodes without a desktop.
arg-default-output = The output: its name or description, or a unique part of either.
cmd-settings = Show the settings that can change while the node runs, and where each comes from: the default, config.toml, or `set`.
cmd-set = Change settings, kept across restarts (config.toml is not touched).
arg-set-settings = SECTION.KEY=VALUE, e.g. receiver.policy=mix or ptp.domain=1.
cmd-reset = Undo `set`: go back to config.toml or the default.
arg-reset-keys = SECTION.KEY, or SECTION for all of its keys.
cmd-routes = List the routes that run and the ones remembered from before.
cmd-disconnect = Stop a route started with `connect` and disconnect the peer's receiver. The route is remembered for the next `connect`.
arg-disconnect-route = The route: its name, or its ID or the start of it (see `routes`).
cmd-forget = Forget a route, so that the next `connect` starts a new one; disconnects it first if it runs.
arg-forget-route = The route: its name, or its ID or the start of it (see `routes`).
cmd-route-layout = Set what the channels of a route's sink carry, and with them how many channels its stream has; the sink starts again, and so does the peer's receiver.
arg-route-layout-route = The route: its name, or its ID or the start of it (see `routes`).
arg-route-layout-layout = ST 2110-30 groups (`ST`, `51`, `71`, `U16`) or channel symbols (`L R C`).
cmd-sources = List the sources the receivers remember: where each plays and how loud.
cmd-layout = Set what the channels of a source carry, for senders that say nothing or say it wrong; a stream playing it restarts.
arg-layout-source = The source: its name, or its ID or the start of it (see `sources`).
arg-layout-layout = ST 2110-30 groups (`51`, `ST,M`, `DM`, `U02`), channel symbols (`L R`, `M1 M2`), or `auto` for what the stream says.
cmd-wire = Play channels of a source or an input on an output, replacing the wire between them. A source with wires plays only on them; what several wires bring into one channel is summed.
arg-wire-from = The source or input: its name, or a source's ID or the start of it (see `sources` and `inputs`).
arg-wire-output = The output: its name or description, or a unique part of either.
arg-wire-channels = Output channel = input channel, each by number (from 1), symbol or label, with a gain in dB after `@` if wanted: `L=R R=L`, `1=U03@-6`, `2=1`. Without any, all channels go where they belong, those folded onto others at -3 dB.
arg-wire-gain = The wire's gain in dB, from -90 to +12; 0 plays the channels as they are.
arg-wire-mute = Make it silent.
cmd-wire-gain = Change the gain of a wire, keeping its channels; the change ramps over 20 ms.
arg-wire-gain-from = The source or input, as for `wire`.
arg-wire-gain-output = The output, as for `wire`.
arg-wire-gain-gain = A gain in dB from -90 to +12 (`-6`, `0`), `mute` or `unmute`.
cmd-unwire = Remove a wire, or some of its channels. A source left without wires is parked until played with `play` or wired again.
arg-unwire-from = The source or input, as for `wire`.
arg-unwire-output = The output, as for `wire`.
arg-unwire-channels = The channel pairs to remove, as for `wire`; all if none.
cmd-patch = Show the wires: what plays on each output channel, and how loud: the peak over the last second of each output channel and of what each wire brings to it.
arg-patch-watch = Refresh every SECONDS (default 1), as a meter.
cmd-forget-source = Forget where a source plays and how loud; if it plays now, it goes to the default output at full volume.
arg-forget-source-source = The source: its name, or its ID or the start of it (see `sources`).

## Errors. Quoted names are what was given on the command line.

watch-too-short = --watch needs at least 0.1 seconds
expected-setting = { $arg }: expected SECTION.KEY=VALUE
expected-key = { $arg }: expected SECTION.KEY
not-a-gain = "{ $gain }" is no gain: give dB, mute or unmute
bad-gain-in-pair = "{ $pair }": "{ $db }" is no gain in dB
bad-pair = "{ $pair }" is not OUT=IN, e.g. L=R, 1=U03 or L=C@-3
no-wire = no wire from { $from } to { $output }
wire-plays-all = the wire plays all channels; `orpheusctl unwire "{ $from }" "{ $output }"` removes it
wire-lacks = the wire has no { $pair }
# $list: routes or sources, the command that lists them.
pick-none = { $list ->
        [routes] no route "{ $query }"; see `orpheusctl routes`
       *[sources] no source "{ $query }"; see `orpheusctl sources`
    }
pick-many = "{ $query }" matches { $list ->
        [routes] routes
       *[sources] sources
    } { $ids }; give the ID
no-source-or-input = no source or input "{ $from }"; see `orpheusctl sources` and `orpheusctl inputs`
from-ambiguous = "{ $from }" names more than one source or input; be more specific
from-ambiguous-id = "{ $from }" names more than one source or input; give a source's ID
no-node = no node "{ $query }"; see `orpheusctl nodes`
not-orpheus = { $query } is not an Orpheus node
no-output = no output matches "{ $query }"; see `orpheusctl outputs`
output-ambiguous = "{ $query }" matches { $outputs }; be more specific
refused = the node refused: { $message } (HTTP { $status })
not-running = orpheusd is not running on this machine
not-running-hint = Start it with orpheusd, or choose a node with --node or ORPHEUS_NODE.
no-node-at = no Orpheus node at { $base }: { $reason }
no-node-at-hint = Is orpheusd running there? Choose a node with --node or ORPHEUS_NODE.
reason-host-not-found = host not found
reason-connection-failed = connection failed
reason-timed-out = timed out
reason-connection-refused = connection refused
reason-connection-reset = connection reset
reason-host-unreachable = host unreachable
reason-network-unreachable = network unreachable
reason-address-not-available = address not available
node-closed = the node closed the connection
bad-node-url = bad node URL { $base }
watch-needs-http = watching needs an http:// node URL, not { $base }

# What orpheusctl was doing when something failed, before the cause.
querying = querying { $url }
connecting-via = connecting to { $peer } via { $url }
connecting = connecting to { $base }
looking-up = looking up { $host }
opening = opening { $url }
reading-reply = reading the reply from { $url }
reading = reading { $what ->
        [nodes] the nodes
        [outputs] the outputs
        [inputs] the inputs
        [routes] the routes
        [sources] the sources
        [wires] the wires
        [wire] the wire
        [levels] the levels
        [settings] the settings
        [status] the status
        [event] an event
       *[events] events
    }
reading-from = reading { $what ->
        [nodes] nodes
        [routes] routes
        [sources] sources
       *[status] status
    } from { $url }
creating-via = creating { $name } via { $url }
removing-via = removing { $what } via { $url }
resetting-via = resetting via { $url }
setting-via = setting { $what } via { $url }
moving-via = moving via { $url }
setting-default-via = setting the default via { $url }
disconnecting-via = disconnecting { $route } via { $url }
forgetting-via = forgetting { $what } via { $url }
setting-layout-via = setting the layout of { $what } via { $url }
wiring-via = wiring { $from } to { $output } via { $url }
setting-gain-via = setting the gain of { $from } → { $output } via { $url }
unwiring = unwiring { $from } from { $output }
unwiring-channels-via = unwiring channels via { $url }
changing-node-via = changing node { $id } via { $url }
changing-settings-via = changing settings via { $url }

## Results of commands.

route-created = route { $id } "{ $name }" → { $to } ({ $receiver ->
        [yes] receiver connected
       *[no] send only
    })
output-created = output { $name }

## Status.

heading-audio = Audio
heading-warnings = Warnings
heading-clock = Clock
heading-loopback = Loopback
heading-routes = Routes
receiver-title = Receiver { $id }
no-audio = no audio system, no streams
system-clock = system clock
ptp-holding-over = PTP stopped, holding over
ptp-not-running = PTP not running, system clock
ptp-settled = PTP, settled
ptp-settling = PTP, settling
# $state: the PTP port's state, as IEEE 1588 names it.
ptp-state = { $state ->
        [listening] listening
        [master] master
        [slave] slave
        [passive] passive
        [uncalibrated] uncalibrated
        [pre_master] pre master
        [faulty] faulty
        [disabled] disabled
       *[other] { $state }
    }
ptp-heading-state = PTP, { $state }
this-node = this node
key-error = error
key-interface = interface
key-identity = identity
key-state = state
key-grandmaster = grandmaster
key-offset = offset
key-delay = delay
key-frequency = frequency
key-steps = steps
key-listen = listen
key-channels = channels
key-output = output
key-mix = mix
key-priority = priority
key-link-offset = link offset
key-level = level
key-margin = margin
key-jitter = jitter
key-packets = packets
key-ignored = ignored
key-buffer = buffer
key-asrc = ASRC
key-problems = problems
frames = { $n } frames
connected-off = connected to { $from }, off
free-port = free, port { $port }
receiving-from = receiving from { $from }
receiving-muted = receiving from { $from }, muted
receiving-ducked = receiving from { $from }, ducked
receiving-ssrc = receiving ssrc { $ssrc }
connected-idle = connected to { $from }, idle
free-any = free, plays any sender
idle = idle
parked = parked
parked-lost = parked: { $output } is gone
parked-unwired = parked: its last wire was removed
held = none: the default output is another node's sink
on-wires = on wires, see `orpheusctl patch`
priority-high = high
silence = silence
silent = silent
none = none
# Counters of problems; $n is shown, $count chooses the plural.
count-lost = { $n } lost
count-late = { $n } late
count-early = { $n } early
count-malformed = { $n } malformed
count-realign = { $count ->
        [one] { $n } realign
       *[other] { $n } realigns
    }
count-underrun = { $count ->
        [one] { $n } underrun
       *[other] { $n } underruns
    }
count-overrun = { $count ->
        [one] { $n } overrun
       *[other] { $n } overruns
    }
count-resync = { $count ->
        [one] { $n } resync
       *[other] { $n } resyncs
    }
count-send-error = { $count ->
        [one] { $n } send error
       *[other] { $n } send errors
    }
# How a route's peer stands, after the peer in the routes of `status`.
peer-disconnected = disconnected, paused
peer-busy = waiting for a free receiver
peer-incompatible = peer can't play this stream
peer-offline = peer offline
peer-unreachable = unreachable
peer-connecting = connecting
peer-idle = idle
peer-off = off
peer-output-gone = its output there is gone
peer-unwired = unwired there
peer-sink-hidden = sink hidden
peer-muted = muted by policy
peer-ducked = ducked
peer-high-priority = high priority

## Tables. Column headers are in capitals.

col-id = ID
col-name = NAME
col-peer = PEER
col-packets = PACKETS
col-buffer = BUFFER
col-asrc = ASRC
col-problems = PROBLEMS
col-output = OUTPUT
col-type = TYPE
col-channels = CHANNELS
col-input = INPUT
col-kind = KIND
col-volume = VOLUME
col-layout = LAYOUT
col-state = STATE
col-priority = PRIORITY
col-setting = SETTING
col-value = VALUE
col-from = FROM
col-address = ADDRESS
col-version = VERSION
col-link = LINK
no-outputs = no outputs
no-inputs = no inputs
no-sources = no sources
no-routes = no routes
no-nodes = no other nodes found
type-set = { $type } (set)
created-on = on { $device } { $channels }
created-on-missing = on { $device } { $channels } (missing)
plays-receivers = { $count ->
        [one] receiver
       *[other] receivers
    } { $ids }
default = default
# $kind: receiver (a stream received) or device (of the audio system).
input-kind = { $kind ->
        [receiver] receiver
        [device] device
       *[other] { $kind }
    }
# $from: where a setting comes from.
setting-from = { $from ->
        [default] default
        [config] config.toml
        [api] set
       *[other] { $from }
    }
playing-on-receiver = playing on receiver { $receiver }
# $days: how many days ago.
ago = { $days ->
        [0] today
        [1] yesterday
       *[other] { $days } days ago
    }
last-played = last played { $ago }
as-sent = as sent
remembered = remembered
remembered-last-used = remembered, last used { $ago }
route-connected = connected
route-idle = idle, claims a receiver when audio plays
route-busy = waiting for a free receiver
route-incompatible = peer can't play this stream
route-offline = peer offline
route-unreachable = peer unreachable
route-disconnected = disconnected, paused
online = online
online-pinned = online, pinned
offline = offline
offline-pinned = offline, pinned
wired = wired
wifi = Wi-Fi

## Wires and the patch.

muted = muted
gain-muted = { $gain }, muted
# $mix: downmix, upmix or remix: how channels are folded onto others.
mix = { $mix ->
        [downmix] downmix
        [upmix] upmix
        [remix] remix
       *[other] { $mix }
    }
mix-layouts = { $mix } { $from } → { $to }
mix-left-out = { $mix }, { $channels } left out
plays-nowhere = { $name } plays nowhere
is-missing = { $name } is missing
no-channels-match = no channels match
waiting-because = waiting, { $why }
no-wires = no wires
heading-waiting = Waiting
all-channels = all
clipped-ago = clipped { $secs } s ago
linking = { $wire } (linking)
