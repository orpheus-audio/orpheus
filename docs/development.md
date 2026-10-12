# Development

## Building

```sh
cargo build --workspace
cargo test --workspace
```

The PipeWire bindings need the PipeWire development headers and clang.

`cargo build --workspace --examples` builds only the examples, not
`orpheusd` and `orpheusctl`.

## Tests

- Unit tests live next to the code.
- `orpheus-engine/tests/rtp_route.rs` runs sender → UDP → receiver on
  localhost, with a fake audio backend in place of PipeWire.
- `drift_is_absorbed` in the same file is ignored by default. It runs two
  minutes with the media clock 500 ppm fast against the sound card and checks
  that ASRC holds the latency:

  ```sh
  cargo test -p orpheus-engine --test rtp_route -- --ignored
  ```

- `orpheusd/src/api.rs` checks that the OpenAPI document lists every route and
  that every schema reference resolves.

## Examples

| Example | Purpose |
|---|---|
| `orpheus-clock/examples/ptp_follow.rs` | Runs only the PTP clock and prints its status once a second: `ptp_follow <interface> [skew_ppm] [offset_ms] [seconds] [priority1]`. |
| `orpheus-pipewire/examples/latency_probe.rs` | Measures the latency of a PipeWire path by playing impulses into one node and detecting them on the monitor of a sink. |

## Two nodes on one machine

Two nodes on one machine need separate network namespaces: each needs its own
ports, and PTP multicast doesn't loop back. Without namespaces, two nodes can
still share one machine with PTP disabled: give them separate `ORPHEUS_HOME`
directories and different `[api] listen` and `[receiver] port` values.

The debug variables `ORPHEUS_DEBUG_CLOCK_SKEW_PPM` and
`ORPHEUS_DEBUG_CLOCK_OFFSET_MS` skew a node's view of the system clock, so
that one machine can play two nodes whose clocks disagree. The real kernel
clock is shared, so this tests the servo and ASRC, not real crystal drift or
network jitter. Use two machines for that.

## OpenAPI

```sh
cargo run -p orpheusd -- --openapi > openapi.json
```

The handlers in `orpheusd/src/api.rs` carry their own OpenAPI annotations
(utoipa), so the document follows the code.

## Conventions

- Rust edition 2024; `cargo fmt` and `cargo clippy --workspace`.
- Platform-independent code goes into `orpheus-core` or `orpheus-engine`, so
  it can be shared with the ESP32 firmware and other platforms later.
- Realtime callbacks don't allocate, lock or make syscalls.
- Update these docs at the end of each milestone.
