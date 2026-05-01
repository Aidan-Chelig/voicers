#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

mkdir -p /tmp/voicers-demo
state_path=/tmp/voicers-demo/bob.json
log_path=/tmp/voicers-demo/bob.log
listen_ip=127.0.0.1

daemon_pid=""
cleanup() {
  if [[ -n "${daemon_pid}" ]] && kill -0 "${daemon_pid}" 2>/dev/null; then
    kill "${daemon_pid}" 2>/dev/null || true
    wait "${daemon_pid}" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

rm -f "${state_path}" "${log_path}"

VOICERS_ALLOW_LOOPBACK_INVITES=1 ./target/debug/voicersd \
  --display-name Bob \
  --control-addr 127.0.0.1:7768 \
  --listen-addr "/ip4/${listen_ip}/tcp/4002" \
  --state-path "${state_path}" \
  --no-bootstrap \
  --no-stun \
  >"${log_path}" 2>&1 &
daemon_pid=$!

for _ in {1..50}; do
  if bash -c 'exec 3<>/dev/tcp/127.0.0.1/7768' >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

if ! bash -c 'exec 3<>/dev/tcp/127.0.0.1/7768' >/dev/null 2>&1; then
  echo "Bob daemon did not become ready. Check ${log_path}" >&2
  exit 1
fi

cat <<EOF
Bob is ready on ${listen_ip}:4002.
Wait for Alice to copy her invite with i, then paste it here with Enter.
EOF

./target/debug/voicers --control-addr 127.0.0.1:7768
