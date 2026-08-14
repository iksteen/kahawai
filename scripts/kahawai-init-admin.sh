#!/bin/sh
# Trusted-local first-run setup. The command prompts on the terminal and sends
# credentials only through the hub data directory's mode-0600 Unix socket.
set -eu

bin=${KAHAWAI_BIN:-kahawai}
if [ -n "${KAHAWAI_CONFIG:-}" ]; then
    exec "$bin" --config "$KAHAWAI_CONFIG" hub init-admin
fi
exec "$bin" hub init-admin
