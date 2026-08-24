# Task runner for picolyzer-tester.
#
# `.cargo/config.toml` sets the default build target to the RP2350, so anything
# that has to run on the host - the tester-core unit tests - needs an explicit
# `--target`. Deriving the triple here keeps it out of the README, which used to
# hardcode `aarch64-apple-darwin` and so did not work for anyone on Linux.

host := `rustc -vV | sed -n 's/^host: //p'`
elf := "target/thumbv8m.main-none-eabihf/release/picolyzer-tester"

# Show the available recipes.
default:
    @just --list

# Format, lint and unit-test. Needs no hardware.
check:
    cargo fmt --all -- --check
    # `--bins`, not `--all-targets`: the firmware is `no_main` and a bare-metal
    # target has no `test` crate, so `--all-targets` fails with E0463.
    cargo clippy --release --bins -- -D warnings
    cargo clippy -p tester-core --target {{ host }} --all-targets -- -D warnings
    cargo test -p tester-core --target {{ host }}

# Exercises whatever firmware is *already on the board*, which is not
# necessarily what was just built - run `just flash` first, or the checks are
# green for a binary you are not shipping.
[doc("check, plus the 59 hardware checks. Needs a Pico on USB.")]
verify: check
    python3 tools/console.py

# Flash over a debug probe. probe-rs names this family RP235x, not RP2350.
flash:
    cargo build --release
    probe-rs download --chip RP235x {{ elf }}
    probe-rs reset --chip RP235x

# Build the release firmware and package it as a versioned .uf2.
uf2:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
    out="picolyzer-tester-v${version}.uf2"
    # `-t elf` because the input has no extension and picotool otherwise
    # refuses it; the family is what makes it an Arm-secure RP2350 image.
    picotool uf2 convert {{ elf }} -t elf "${out}" --family rp2350-arm-s
    picotool info "${out}"

# Gated on `check flash verify`, in that order: lint and unit-test, put the
# binary being released onto the board, then run the 59 hardware checks against
# that binary rather than against whatever happened to be flashed. This needs a
# debug probe as well as USB. The hardware checks are this project's real
# verification and no CI runner can perform them.
#
# The GitHub release is left as a draft on purpose: publishing is outward-facing
# and the notes deserve a human.
[doc("Cut a release. LEVEL is major, minor or patch.")]
release level: check flash verify
    #!/usr/bin/env bash
    set -euo pipefail
    # `--no-confirm`: the gate is the check/flash/verify chain above, not a y/n
    # prompt, and a prompt makes the recipe unusable from anything but a TTY.
    cargo release {{ level }} --execute --no-confirm
    version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
    just uf2
    gh release create "v${version}" "picolyzer-tester-v${version}.uf2" \
        --draft --generate-notes --title "v${version}"
    echo
    echo "Draft release v${version} created. Review the notes, then publish it."
