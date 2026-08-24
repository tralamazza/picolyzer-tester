#!/usr/bin/env bash
#
# Detect codegen drift in the bit-banged I2C path.
#
# crates/tester-core/src/i2c_timing.rs fits two constants - cycles per delay
# count, and fixed cycles per bit - to the machine code LLVM emits for
# I2cWire::byte. They are properties of the generated code, not of the RP2350,
# and the firmware reports `actual_hz` from them. A compiler, HAL or inlining
# change that reshapes that function turns `actual_hz` into a confident lie,
# which is the worst failure this project can have: the whole point of the
# device is that its replies can be trusted over the analyzer's.
#
# Nothing else catches it. The 46 host tests check the model's arithmetic, not
# whether the model still matches silicon - only a bench measurement can do
# that. So this hashes the instruction bytes of that one function and fails
# loudly when they move, to say *when* a re-measurement is owed.
#
# The hash covers opcode bytes only - no addresses, no symbol names - so it
# survives the function being relocated and the legacy-to-v0 mangling change.
# That is only valid while the function has no pc-relative literal loads, whose
# pool entries would hold absolute addresses; the check below enforces it.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
golden=$here/codegen-guard.sha256
elf=$here/../target/thumbv8m.main-none-eabihf/release/picolyzer-tester
record=no

while [[ $# -gt 0 ]]; do
    case $1 in
        --record) record=yes ;;
        -*) echo "codegen-guard: unknown option $1" >&2; exit 1 ;;
        *) elf=$1 ;;
    esac
    shift
done

if [[ ! -f $elf ]]; then
    echo "codegen-guard: no ELF at $elf - run 'cargo build --release' first" >&2
    exit 1
fi

sysroot=$(rustc --print sysroot)
host=$(rustc -vV | sed -n 's/^host: //p')
objdump=$sysroot/lib/rustlib/$host/bin/llvm-objdump
if [[ ! -x $objdump ]]; then
    echo "codegen-guard: llvm-objdump missing - run 'rustup component add llvm-tools'" >&2
    exit 1
fi

# Match the readable fragment, not a fixed symbol: the mangling scheme and the
# hash suffix both change between toolchain versions.
sym=$("$objdump" --syms "$elf" | grep -oE '[_A-Za-z0-9]*I2cWire[_A-Za-z0-9]*byte[_A-Za-z0-9]*' | head -1)
if [[ -z $sym ]]; then
    echo "codegen-guard: could not find I2cWire::byte in $elf" >&2
    exit 1
fi

disasm=$("$objdump" --disassemble-symbols="$sym" "$elf")

if grep -qE '\[pc, ' <<<"$disasm"; then
    echo "codegen-guard: I2cWire::byte now has pc-relative literal loads." >&2
    echo "  The byte hash is no longer position-independent; this script needs" >&2
    echo "  to normalise the constant pool before it can be trusted again." >&2
    exit 1
fi

# Keep only the opcode-byte column: lines look like "100098e8: b5f0 <tab> push ..."
bytes=$(awk -F'\t' '/^[0-9a-f]+:/ {print $1}' <<<"$disasm" | sed -E 's/^[0-9a-f]+: //; s/ +$//')
actual=$(printf '%s' "$bytes" | shasum -a 256 | cut -d' ' -f1)

if [[ $record == yes ]]; then
    printf '%s\n' "$actual" >"$golden"
    echo "codegen-guard: recorded $actual"
    exit 0
fi

expected=$(head -1 "$golden" 2>/dev/null || true)
if [[ -z $expected ]]; then
    echo "codegen-guard: no golden hash in $golden; record one with --record" >&2
    exit 1
fi

if [[ $actual == "$expected" ]]; then
    echo "codegen-guard: I2cWire::byte unchanged ($actual)"
    exit 0
fi

cat >&2 <<EOF
codegen-guard: I2cWire::byte codegen CHANGED

  expected $expected
  actual   $actual
  rustc    $(rustc -V)

The I2C timing constants in crates/tester-core/src/i2c_timing.rs were fitted to
the old machine code, so \`actual_hz\` may now be wrong. This is not a bug to
paper over: re-measure SCL on a logic analyzer at 10 k, 50 k, 100 k and 400 kHz,
re-fit CYCLES_PER_QUARTER_X1000 and FIXED_CYCLES_PER_BIT if they moved, then
record the new hash with:

    tools/codegen-guard.sh --record
EOF
exit 1
