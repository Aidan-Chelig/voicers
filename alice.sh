#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

mkdir -p /tmp/voicers-demo
state_path=/tmp/voicers-demo/alice.json
log_path=/tmp/voicers-demo/alice.log
room_name=local-demo
listen_ip=127.0.0.1

send_control_request() {
  local host=$1
  local port=$2
  local payload=$3
  local response=""
  exec 3<>"/dev/tcp/${host}/${port}"
  printf '%s\n' "${payload}" >&3
  IFS= read -r response <&3 || true
  exec 3>&-
  exec 3<&-
  printf '%s\n' "${response}"
}

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
  --display-name Alice \
  --control-addr 127.0.0.1:7767 \
  --listen-addr "/ip4/${listen_ip}/tcp/4001" \
  --state-path "${state_path}" \
  --no-bootstrap \
  --no-stun \
  >"${log_path}" 2>&1 &
daemon_pid=$!

for _ in {1..50}; do
  if bash -c 'exec 3<>/dev/tcp/127.0.0.1/7767' >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

if ! bash -c 'exec 3<>/dev/tcp/127.0.0.1/7767' >/dev/null 2>&1; then
  echo "Alice daemon did not become ready. Check ${log_path}" >&2
  exit 1
fi

send_control_request \
  127.0.0.1 \
  7767 \
  "{\"CreateRoom\":{\"room_name\":\"${room_name}\"}}" >/dev/null

cat <<EOF
Alice is in room ${room_name} on ${listen_ip}:4001.
Press i to copy the invite.
When Bob joins, press y to allow once or w to whitelist.
EOF

./target/debug/voicers --control-addr 127.0.0.1:7767
