# chunky_mcu_updater. `just` lists the recipes.

default:
    @just --list

# All tests: unit tests, planner properties, security cases and the power-cut sweeps (CHUNKY_SWEEP_SEEDS=500 digs deeper)
test:
    cargo test --workspace
    just test-c

# Parser fuzzing at scale (SIM-3): every untrusted parser of format 2 and the apply engine on hostile plans with power cuts
fuzz cases="20000":
    CHUNKY_FUZZ_CASES={{cases}} cargo test --release -p chunky_sim --test fuzz

# The C reader generated from chunky_format's types against its golden vectors (PROTO-3)
test-c:
    mkdir -p target
    cc -std=c99 -Wall -Wextra -Werror -pedantic -Wconversion -Wsign-conversion -o target/chunky_format_c_test chunky_format/c/test.c
    target/chunky_format_c_test

# Regenerate the golden vectors and c/golden.h from the fixed firmware pair (PROTO-3); a test fails when they differ
vectors:
    cargo run -q -p chunky_server --example vectors

# clippy (warnings are errors), formatting, and the firmware-target builds of both trust options
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --check
    just check-fw
    just lint-bench-fw
    just bench-fw
    just bench-lint
    cd chunky_format/size_bench && cargo fmt --check

# The no_std crates build for Cortex-M0+ and M4F/M7, signed and unsigned, with a series-* feature or chip-custom;
# chunky with ww_ecc_flash's errors on H7 and G0
check-fw:
    cargo check -q -p chunky_swap --target thumbv6m-none-eabi --features ed25519,chunky/series-stm32g0
    cargo check -q -p chunky_swap --target thumbv6m-none-eabi --features unsigned,chunky/chip-custom
    cargo check -q -p chunky_swap --target thumbv7em-none-eabihf --features ed25519,chunky/series-stm32g4
    cargo check -q -p chunky_swap --target thumbv7em-none-eabihf --features unsigned,chunky/chip-custom
    cargo check -q -p chunky_bank --target thumbv6m-none-eabi --features ed25519,chunky/chip-custom
    cargo check -q -p chunky_bank --target thumbv7em-none-eabihf --features ed25519,chunky/series-stm32h7
    cargo check -q -p chunky_bank --target thumbv7em-none-eabihf --features unsigned,chunky/series-stm32h7
    cargo check -q -p chunky_format --target thumbv6m-none-eabi
    cargo check -q -p chunky --target thumbv7em-none-eabihf --features unsigned,series-stm32h7,ww-ecc-flash,ww_ecc_flash/stm32h7
    cargo check -q -p chunky --target thumbv6m-none-eabi --features unsigned,series-stm32g0,ww-ecc-flash,ww_ecc_flash/stm32g0
    cargo check -q -p chunky_ww --target thumbv7em-none-eabihf --features unsigned,chunky/series-stm32h7
    cargo check -q -p chunky_ww --target thumbv6m-none-eabi --features ed25519,chunky/series-stm32g0

# Power-cut bench firmware (HW-3): both bootloaders and apps A and B, raw images and ELFs in bench/fw/out
bench-fw:
    just _bench-fw bench_bootloader stock boot-stock boot
    just _bench-fw bench_bootloader guarded boot-guarded boot
    just _bench-fw bench_app image-a app-a app-a
    just _bench-fw bench_app image-b app-b app-b

_bench-fw pkg feature name kind:
    mkdir -p bench/fw/out
    cd bench/fw && cargo build -q --release -p {{pkg}} --features {{feature}}
    cp bench/fw/target/thumbv6m-none-eabi/release/{{pkg}} bench/fw/out/{{name}}.elf
    cargo run -q -p bench_fw_image -- bench/fw/out/{{name}}.elf bench/fw/out/{{name}}.bin {{kind}}

# clippy and formatting of the bench firmware workspace, every build of it
lint-bench-fw:
    cd bench/fw && cargo fmt --check
    cd bench/fw && cargo clippy -q --release -p bench_bootloader --features stock -- -D warnings
    cd bench/fw && cargo clippy -q --release -p bench_bootloader --features guarded -- -D warnings
    cd bench/fw && cargo clippy -q --release -p bench_app --features image-a -- -D warnings
    cd bench/fw && cargo clippy -q --release -p bench_app --features image-b -- -D warnings

# With tpm on PATH: tpm install (kept-releases stack, one copy on PATH); otherwise: cargo install
# Build the `chunky` command line tool (deploy profile) and install it on this PC
install:
    cargo build --profile deploy --locked -p chunky_cli
    if command -v tpm >/dev/null; then tpm install target/deploy/chunky; else cargo install --locked --path chunky_cli; fi

# chunky_format, chunky_ww and chunky_ww_host build against the sibling wire_weaver checkout: bring it up to its main
deps-pull:
    git -C ../wire_weaver pull --ff-only

# Refuses on local changes (names them, never discards them), before anything is pulled.
# Bring this PC up to main: pull --ff-only, the sibling wire_weaver, install, check --version against main
deploy:
    #!/usr/bin/env bash
    set -eu
    dirty=$(git status --porcelain)
    if [ -n "$dirty" ]; then echo "local changes, not deploying:"; echo "$dirty"; exit 1; fi
    git pull --ff-only
    just deps-pull
    just install
    chunky --version | grep -q "($(git rev-parse --short HEAD)" || { echo "chunky --version does not show $(git rev-parse --short HEAD):"; chunky --version; exit 1; }
    chunky --version

# The power-cut bench harness (bench/host, own workspace; the glitch git dependency is fetched by git over ssh)
bench-cargo := "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo"

# Tests of the bench harness: run loop, sweep and CSV on the simulated bench (no hardware)
bench-test:
    {{bench-cargo}} test --manifest-path bench/host/Cargo.toml

# clippy (warnings are errors) and formatting of the bench harness
bench-lint:
    {{bench-cargo}} clippy --manifest-path bench/host/Cargo.toml --all-targets -- -D warnings
    {{bench-cargo}} fmt --all --manifest-path bench/host/Cargo.toml --check

# Measure mode on the bench (bench/README.md M1-M3), e.g. `just bench-measure --pre-trigger-us 30000 --cut 0`
bench-measure *ARGS:
    {{bench-cargo}} run -q --release --manifest-path bench/host/Cargo.toml -- measure {{ARGS}}

# A sweep into bench/results/NAME, e.g. `just bench-sweep --name s1 --from 3ms --to 30ms --pre-trigger-us 12000` (--dry-run: simulated)
bench-sweep *ARGS:
    {{bench-cargo}} run -q --release --manifest-path bench/host/Cargo.toml -- sweep {{ARGS}}

# Code size of the plan readers (PROTO-3) on Cortex-M0 and RV32IMC: empty floor, format 2
# through the fixed-layout fast path and through shrink_wrap's BufReader. Text includes a 2048-byte buffer.
size-bench:
    cd chunky_format/size_bench && cargo build -q --release --target-dir target --target thumbv6m-none-eabi
    cd chunky_format/size_bench && cargo build -q --release --target-dir target --target riscv32imc-unknown-none-elf
    cd chunky_format/size_bench/target/thumbv6m-none-eabi/release && llvm-size empty fixed sw
    cd chunky_format/size_bench/target/riscv32imc-unknown-none-elf/release && llvm-size empty fixed sw
