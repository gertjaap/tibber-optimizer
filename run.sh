#!/usr/bin/env bash
set -e

CONFIG_PATH=/data/options.json

# Export environment variables from config if needed
export RUST_LOG="${RUST_LOG:-tibber_optimizer=info}"

exec /app/tibber-optimizer
