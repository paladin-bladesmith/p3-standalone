#!/usr/bin/env bash
set -euo pipefail

pids=()

start() {
    "$@" &
    pids+=("$!")
}

cleanup() {
    # echo "Terminating children..."
    for pid in "${pids[@]}"; do
        # skip if already gone
        if kill -0 "$pid" 2>/dev/null; then
            # get process group id
            pgid=$(ps -o pgid= "$pid" 2>/dev/null | tr -d ' ')
            if [[ -n "$pgid" ]]; then
                # kill the whole process group
                kill -TERM -- -"${pgid}" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
            else
                kill -TERM "$pid" 2>/dev/null || true
            fi
        fi
    done

    # wait for background jobs to exit
    wait "${pids[@]}" 2>/dev/null || true
}

trap 'cleanup; exit' SIGINT SIGTERM EXIT

start cargo run --bin p3-standalone -- --rpc-servers http://127.0.0.1:8899 --websocket-servers ws://127.0.0.1:8900 --p3-addr 127.0.0.1:4825 --p3-mev-addr 127.0.0.1:4826 --grpc-bind-ip 127.0.0.1:6001
start cargo run --bin p3-standalone -- --rpc-servers http://127.0.0.1:8899 --websocket-servers ws://127.0.0.1:8900 --p3-addr 127.0.0.1:4823 --p3-mev-addr 127.0.0.1:4824 --grpc-bind-ip 127.0.0.1:6000
start cargo run --bin p3-standalone -- --rpc-servers http://127.0.0.1:8899 --websocket-servers ws://127.0.0.1:8900 --p3-addr 127.0.0.1:4821 --p3-mev-addr 127.0.0.1:4822 --grpc-bind-ip 127.0.0.1:5999

# wait until they're done or until Ctrl-C
wait "${pids[@]}"
