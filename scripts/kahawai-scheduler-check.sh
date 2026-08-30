#!/usr/bin/env bash
# Verify the universal mediahost scheduler's priority, preemption, resource
# concurrency, configuration validation and protocol-4.1 hint gates.
#
# Usage: scripts/kahawai-scheduler-check.sh
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"

cargo test -p kahawai-mediahost scheduler::tests::
cargo test -p kahawai-runtime scheduler_
cargo test -p kahawai-proto protocol_four_one_keeps_inherited_gates_open_and_adds_hints
