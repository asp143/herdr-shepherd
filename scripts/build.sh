#!/bin/sh
set -eu

cargo build --release --locked
mkdir -p bin
cp target/release/herdr-shepherd bin/
