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
