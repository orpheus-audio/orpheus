# Profiles: dev (debug build, fastest to build), release (default), optimized
# (whole-program optimization for this machine's CPU; slow to build, and the
# binaries do not run on other machines).

# List the recipes.
default:
    @just --list

# Build orpheusd and orpheusctl and let orpheusd bind the PTP ports.
build profile="release":
    {{ if profile == "optimized" { "RUSTFLAGS='-C target-cpu=native'" } else { "" } }} cargo build --profile {{profile}} -p orpheusd -p orpheusctl
    @just setcap {{profile}}

# Build and run orpheusd.
run profile="release" *args: (build profile)
    target/{{ if profile == "dev" { "debug" } else { profile } }}/orpheusd {{args}}

# Let orpheusd bind the PTP ports 319 and 320, unless it already can.
setcap profile="release":
    #!/bin/sh
    set -eu
    bin=target/{{ if profile == "dev" { "debug" } else { profile } }}/orpheusd
    if [ "$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start)" -le 319 ]; then
        exit 0
    fi
    if getcap "$bin" | grep -q cap_net_bind_service; then
        exit 0
    fi
    echo "setcap needs sudo: PTP binds the privileged ports 319 and 320"
    sudo setcap cap_net_bind_service=+ep "$bin"

# Format, lint and test the workspace.
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
