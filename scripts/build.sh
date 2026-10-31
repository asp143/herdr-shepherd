#!/bin/sh
set -eu

cargo build --release --locked --target-dir target
mkdir -p bin
cp target/release/herdr-shepherd bin/
