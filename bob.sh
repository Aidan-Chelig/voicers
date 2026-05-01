#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

mkdir -p /tmp/voicers-demo

daemon_pid=""
cleanup() {
  if [[ -n "${daemon_pid}" ]] && kill -0 "${daemon_pid}" 2>/dev/null; then
    kill "${daemon_pid}" 2>/dev/null || true
    wait "${daemon_pid}" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

./target/debug/voicersd \
  --display-name Bob \
  --control-addr 127.0.0.1:7768 \
  --listen-addr /ip4/127.0.0.1/tcp/4002 \
  --state-path /tmp/voicers-demo/bob.json \
  --no-capture \
  --no-bootstrap \
  --no-stun \
  >/tmp/voicers-demo/bob.log 2>&1 &
daemon_pid=$!

for _ in {1..50}; do
  if bash -c 'exec 3<>/dev/tcp/127.0.0.1/7768' >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

if ! bash -c 'exec 3<>/dev/tcp/127.0.0.1/7768' >/dev/null 2>&1; then
  echo "Bob daemon did not become ready. Check /tmp/voicers-demo/bob.log" >&2
  exit 1
fi

./target/debug/voicers --control-addr 127.0.0.1:7768
