#!/usr/bin/env bash
#
# The Shadow compatibility spike for the non-wasm reference peer: can Node +
# libwebrtc (@roamhq/wrtc) run under the Shadow discrete-event network
# simulator? Each stage isolates a failure layer, so a failure identifies the
# incompatible component rather than the whole stack:
#
#   1  node-hello      V8/Node boots and prints under Shadow
#   2  wrtc-loopback   the wrtc native addon + libwebrtc ICE/DTLS/SCTP connect
#                      two in-process peers on one simulated host
#   3  reference-pair  two reference-peer processes on separate simulated hosts
#                      run interop-handshake through conformance-signalingd
#                      over the simulated network
#   4  reference-x-wasmtime  (optional; runs when the artifacts exist) the
#                      reference peer against the native wasmtime
#                      conformance-peer, same topology as stage 3
#
# Shadow is x86-64-only, so this spike must run on an x86-64 Linux machine
# with `shadow` on PATH (scripts/download-shadow.sh or scripts/build-shadow.sh)
# and `npm install` done in this directory. Stage 4 additionally needs
# `just conformance::build-guest`, `cargo build --release -p
# conformance-adapter-wasmtime --bin conformance-peer`, and `cargo build -p
# conformance-signalingd` (stage 3 needs the signalingd build too).
#
# Usage: shadow-spike.sh [stage...]   # default: every runnable stage

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "${HERE}/../../.." && pwd)"
NODE_BIN="$(command -v node)" || { echo "node not found" >&2; exit 1; }
SHADOW_BIN="${SHADOW_BIN:-shadow}"
command -v "${SHADOW_BIN}" >/dev/null || { echo "shadow not found on PATH" >&2; exit 1; }

SPIKE_DIR="${ROOT}/target/shadow-ref-spike"
SIGNALING_BIN="${ROOT}/target/debug/conformance-signalingd"
PEER_BIN="${ROOT}/target/release/conformance-peer"
GUEST="${ROOT}/conformance/guest/build/conformance-guest.component.wasm"

STAGES=("$@")
if [ ${#STAGES[@]} -eq 0 ]; then
  STAGES=(1 2 3)
  [ -x "${PEER_BIN}" ] && [ -f "${GUEST}" ] && STAGES+=(4)
fi

# Emit the shared config preamble (mirrors conformance-shadow's settings).
preamble() {
  cat <<EOF
general:
  stop_time: 300s
  model_unblocked_syscall_latency: true
network:
  graph:
    type: 1_gbit_switch
hosts:
EOF
}

# emit_host NAME IP PATH START EXPECTED_RUNNING ARGS...
emit_host() {
  local name="$1" ip="$2" path="$3" start="$4" running="$5"
  shift 5
  local args=""
  for a in "$@"; do
    a="${a//\\/\\\\}"
    a="${a//\"/\\\"}"
    args="${args:+${args}, }\"${a}\""
  done
  cat <<EOF
  ${name}:
    ip_addr: ${ip}
    network_node_id: 0
    processes:
    - path: ${path}
      args: [${args}]
      start_time: ${start}
EOF
  [ "${running}" = yes ] && echo "      expected_final_state: running"
  return 0
}

# run_stage NAME EXPECTED_HOST... — run ${SPIKE_DIR}/NAME.yaml and require the
# result line {"tag":"pass"} on each expected host's stdout.
run_stage() {
  local name="$1"
  shift
  local data="${SPIKE_DIR}/${name}-data"
  rm -rf "${data}"
  echo "== stage ${name} =="
  "${SHADOW_BIN}" --parallelism 4 --data-directory "${data}" \
    "${SPIKE_DIR}/${name}.yaml" > "${SPIKE_DIR}/${name}.log" 2>&1
  local shadow_rc=$?
  local ok=0
  for host in "$@"; do
    local out
    out="$(cat "${data}/hosts/${host}"/*.stdout 2>/dev/null | grep -v '^$' | tail -1)"
    echo "   ${host}: ${out:-<no output>}"
    [ "$(printf '%s' "${out}" | head -c 14)" = '{"tag":"pass"}' ] || ok=1
  done
  if [ ${ok} -ne 0 ] || [ ${shadow_rc} -ne 0 ]; then
    echo "   FAIL (shadow rc=${shadow_rc}; log: ${SPIKE_DIR}/${name}.log)"
    echo "   --- last lines of the shadow log:"
    tail -20 "${SPIKE_DIR}/${name}.log" | sed 's/^/   /'
    for host in "$@"; do
      for err in "${data}/hosts/${host}"/*.stderr; do
        [ -s "${err}" ] || continue
        echo "   --- last lines of ${host} stderr:"
        tail -10 "${err}" | sed 's/^/   /'
      done
    done
    return 1
  fi
  echo "   PASS"
}

mkdir -p "${SPIKE_DIR}"
failures=0

for stage in "${STAGES[@]}"; do
  case "${stage}" in
    1)
      { preamble
        emit_host hello 11.0.0.1 "${NODE_BIN}" 1s no \
          -e 'console.log(JSON.stringify({tag:"pass"}))'
      } > "${SPIKE_DIR}/1-node-hello.yaml"
      run_stage 1-node-hello hello || failures=$((failures + 1))
      ;;
    2)
      { preamble
        emit_host loopback 11.0.0.1 "${NODE_BIN}" 1s no \
          "${HERE}/wrtc-loopback.mjs"
      } > "${SPIKE_DIR}/2-wrtc-loopback.yaml"
      run_stage 2-wrtc-loopback loopback || failures=$((failures + 1))
      ;;
    3)
      [ -x "${SIGNALING_BIN}" ] || { echo "missing ${SIGNALING_BIN} (cargo build -p conformance-signalingd)" >&2; exit 1; }
      { preamble
        emit_host sig 11.0.0.1 "${SIGNALING_BIN}" 0s yes \
          --host 11.0.0.1 --port 8080
        for role_ip in offerer:11.0.0.2 answerer:11.0.0.3; do
          emit_host "${role_ip%%:*}" "${role_ip##*:}" "${NODE_BIN}" 2s no \
            "${HERE}/peer.mjs" --test interop-handshake \
            --role "${role_ip%%:*}" --server http://11.0.0.1:8080 --room r \
            --message-count 16 --message-size 512
        done
      } > "${SPIKE_DIR}/3-reference-pair.yaml"
      run_stage 3-reference-pair offerer answerer || failures=$((failures + 1))
      ;;
    4)
      [ -x "${SIGNALING_BIN}" ] || { echo "missing ${SIGNALING_BIN}" >&2; exit 1; }
      [ -x "${PEER_BIN}" ] && [ -f "${GUEST}" ] || { echo "missing ${PEER_BIN} or ${GUEST}" >&2; exit 1; }
      { preamble
        emit_host sig 11.0.0.1 "${SIGNALING_BIN}" 0s yes \
          --host 11.0.0.1 --port 8080
        emit_host offerer 11.0.0.2 "${NODE_BIN}" 2s no \
          "${HERE}/peer.mjs" --test interop-handshake \
          --role offerer --server http://11.0.0.1:8080 --room r \
          --message-count 16 --message-size 512
        emit_host answerer 11.0.0.3 "${PEER_BIN}" 2s no \
          --guest "${GUEST}" --test interop-handshake \
          --role answerer --server http://11.0.0.1:8080 --room r \
          --message-count 16 --message-size 512 \
          --bind-addr 11.0.0.3 --disable-mdns
        # Arm the native peer's Shadow syscall shim (see
        # adapters/wasmtime/src/bin/peer/shadow_shim.rs).
        echo "      environment: { CONFORMANCE_SHADOW_SYSCALL_SHIM: \"1\" }"
      } > "${SPIKE_DIR}/4-reference-x-wasmtime.yaml"
      run_stage 4-reference-x-wasmtime offerer answerer || failures=$((failures + 1))
      ;;
    *)
      echo "unknown stage ${stage}" >&2
      exit 2
      ;;
  esac
done

if [ ${failures} -ne 0 ]; then
  echo "== ${failures} stage(s) failed =="
  exit 1
fi
echo "== all stages passed =="
