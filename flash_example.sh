#!/bin/sh

set -e
cargo build --release --example pi_pico
cd target/thumbv6m-none-eabi/release/examples
pico_flasher pi_pico
