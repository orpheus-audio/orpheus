# Names Orpheus shows in the audio system's mixers.

# A sink to another node while that node can't play the stream.
sink-unavailable = { $name } (unavailable)
# The local test route (`[loopback]`) and its playback stream.
loopback = Orpheus loopback
loopback-playback = { $name } (playback)
# The nodes that measure an output and that play a wire, seen in graph
# tools such as qpwgraph.
meter = Orpheus meter { $output }
wire = Orpheus wire { $from } → { $to }

## Warnings, shown by `orpheusctl status` and in the status JSON. Each is
## one line; Fluent would keep line breaks.

# Why PTP is not running when it gives no reason.
ptp-not-started = not started
warning-ptp-holdover = PTP stopped ({ $why }): the clock keeps the grandmaster's last time and rate and slowly drifts from the other nodes; PTP starts again by itself
warning-ptp-down = PTP is not running ({ $why }): the node runs on the system clock and plays out of step with the other nodes; PTP starts by itself once it can
warning-ptp-wifi = PTP runs over Wi-Fi ({ $interface }): the clock may not settle, and rooms drift apart
# $role: "master" while this node is the grandmaster, else "other".
warning-wifi-grandmaster = { $role ->
        [master] this node is the PTP grandmaster over Wi-Fi ({ $interface }): nodes that follow its clock may not settle; give a wired node a lower ptp.priority1
       *[other] this node may become the PTP grandmaster over Wi-Fi ({ $interface }): nodes that follow its clock may not settle; give a wired node a lower ptp.priority1
    }
warning-ptp-delay = the path to the PTP grandmaster takes { $ms } ms, wired it is well below { $limit } ms: { $cause }
warning-ptp-steps = the PTP clock stepped { $count } times in the last minute instead of settling: the path to the grandmaster is slow or unsteady; { $cause }

# The likely cause of a slow or unsteady path, completing the warnings.
cause-unknown = Wi-Fi or a congested link on the way
cause-wifi = this node is on Wi-Fi ({ $interface })
cause-wifi-link = this node is on Wi-Fi ({ $interface }, { $link })
cause-wifi-weak = this node's Wi-Fi is weak ({ $interface }, { $link }); move it or the access point closer
# What the Wi-Fi interface reports of its link to the access point.
wifi-link = signal { $dbm } dBm, { $mbit } Mbit/s
wifi-link-signal = signal { $dbm } dBm
wifi-link-rate = { $mbit } Mbit/s

# Who sends a stream, when the receiver doesn't know its name.
the-sender = the sender
warning-receiver-multicast-wifi = receiver { $receiver } joins multicast group { $group } over Wi-Fi ({ $interface }), which the access point sends at its lowest rate to every client; use a wired link or unicast
warning-receiver-wifi = receiver { $receiver } gets { $from } over Wi-Fi ({ $interface }); expect dropouts unless link_offset_ms is raised
warning-receiver-peer-wifi = { $peer } sends to receiver { $receiver } from Wi-Fi
warning-jitter = packets from { $from } arrive spread over up to { $ms } ms, wired it is well below { $limit } ms: { $cause }; raise link_offset_ms if `late` rises
warning-held = receiver { $receiver } plays nowhere: the default output is a sink to another node, and streams are passed on only when told; choose an output with `orpheusctl play { $receiver } <output>` or set [receiver] default_output
warning-clipping = { $output } reached full scale: what plays on it adds up to more than it can play, and clips; turn its wires down (`orpheusctl wire-gain`)

warning-peer-busy = route { $route }: the peer has no free receiver; it waits for one
warning-peer-incompatible = route { $route }: none of the peer's receivers can play the stream
warning-sender-multicast-wifi = route { $route } sends to multicast group { $group } over Wi-Fi ({ $interface }), which the access point sends at its lowest rate to every client; use a wired link or unicast
warning-sender-wifi = route { $route } goes out over Wi-Fi ({ $interface }); expect dropouts
warning-sender-peer-wifi = route { $route }: { $peer } is on Wi-Fi; expect dropouts
warning-peer-jitter = route { $route }: the peer reports the packets arriving spread over more than { $limit } ms: Wi-Fi or a congested link on the way

-power-save-holds = the access point holds this node's packets until it wakes, up to 100 ms and more, so the clock and streams stutter
-power-save-for-good = turn it off for good: NetworkManager `wifi.powersave = 2`, TLP `WIFI_PWR_ON_BAT=off`; see the troubleshooting docs
warning-power-save-no-capability = Wi-Fi power saving is on for { $interface }: { -power-save-holds }; the node turns it off itself if it may (`sudo setcap cap_net_bind_service,cap_net_admin=ep { $exe }`), or { -power-save-for-good }
warning-power-save-unsupported = Wi-Fi power saving is on for { $interface }, and its driver can't turn it off: { -power-save-holds }; use a wired link or another Wi-Fi adapter
warning-power-save-failed = Wi-Fi power saving is on for { $interface }, and turning it off failed ({ $error }): { -power-save-holds }; { -power-save-for-good }
warning-power-save-turned-on-again = something keeps turning Wi-Fi power saving on for { $interface } (NetworkManager when it connects, TLP on battery); the node turns it off again within 30 s, but meanwhile { -power-save-holds }; { -power-save-for-good }

## Errors in settings: in config.toml as the node starts, and in changes
## over the API (`orpheusctl set`), in the node's language.

setting-range = { $key } must be { $min } to { $max }
setting-empty = { $key } must not be empty
setting-above-zero = { $key } must be above 0
setting-thresholds = [ptp] step_threshold_us and settle_threshold_us must be above 0
setting-language = [ui] language "{ $language }" is neither auto nor a language tag, such as de
setting-ports = [receiver] port + 2 × (max − 1) must stay below 65536
setting-block-both = set either block_frames or cycle_ms, not both
setting-block = the block must be { $min } to { $max } frames (at 48 kHz { $min_ms } to { $max_ms } ms), not { $frames }
settings-not-object = the settings must be a JSON object
settings-section-not-object = { $section } must be an object
settings-unknown-section = unknown section { $section }
# $error: what the JSON parser says, in English.
settings-invalid = invalid settings: { $error }
