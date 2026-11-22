#!/usr/bin/env bash
# gRPC interoperability: giong-grpc against grpc-go, both directions, with
# the test cases of
# github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md.
#
#   giong-grpc/interop/run.sh
#
# Needs Go (1.24 or later) and network access the first time, to fetch
# grpc-go ($GRPC_GO, pinned below) and grpcurl. For each driver in DRIVERS
# (default "io_uring epoll") it runs:
#
#   1. grpc-go's interop client against giong-grpc's server: every case
#      grpc-go implements.
#   2. giong-grpc's interop client against grpc-go's interop server: the
#      same cases. grpc-go's server does not implement the four
#      *_compressed_* cases, so those run against giong-grpc's own server
#      in step 4.
#   3. gzip against grpc-go (interop/go: grpc-go with its gzip compressor
#      registered, which its interop programs leave out), both directions.
#   4. giong-grpc's client against giong-grpc's server: all eighteen cases.
#   5. grpcurl, which knows the server only through reflection: list,
#      describe, a call, and the health service.
#
# Exits with 0 only if every case passed.
set -euo pipefail
cd "$(dirname "$0")/../.."

GRPC_GO="${GRPC_GO:-v1.75.1}"
GRPCURL="${GRPCURL:-v1.9.3}"
drivers="${DRIVERS:-io_uring epoll}"
base_port="${PORT:-10100}"
bin="$PWD/target/grpc-interop/bin"
mkdir -p "$bin"

# The cases grpc-go's interop client and server implement.
go_cases="empty_unary large_unary client_streaming server_streaming ping_pong
empty_stream cancel_after_begin cancel_after_first_response
timeout_on_sleeping_server custom_metadata status_code_and_message
special_status_message unimplemented_method unimplemented_service"

if [ ! -x "$bin/go-client-$GRPC_GO" ] || [ ! -x "$bin/go-server-$GRPC_GO" ]; then
    tmp="$(mktemp -d)"
    (cd "$tmp" && GOBIN="$tmp" go install "google.golang.org/grpc/interop/client@$GRPC_GO" \
        && GOBIN="$tmp" go install "google.golang.org/grpc/interop/server@$GRPC_GO")
    mv "$tmp/client" "$bin/go-client-$GRPC_GO"
    mv "$tmp/server" "$bin/go-server-$GRPC_GO"
    rm -rf "$tmp"
fi
if [ ! -x "$bin/grpcurl-$GRPCURL" ]; then
    tmp="$(mktemp -d)"
    (cd "$tmp" && GOBIN="$tmp" go install "github.com/fullstorydev/grpcurl/cmd/grpcurl@$GRPCURL")
    mv "$tmp/grpcurl" "$bin/grpcurl-$GRPCURL"
    rm -rf "$tmp"
fi
(cd giong-grpc/interop/go && go build -o "$bin/gzip-peer" .)
cargo build --release -p giong-grpc --example interop_server --example interop_client
giong_server="$PWD/target/release/examples/interop_server"
giong_client="$PWD/target/release/examples/interop_client"
go_client="$bin/go-client-$GRPC_GO"
go_server="$bin/go-server-$GRPC_GO"
grpcurl="$bin/grpcurl-$GRPCURL"

pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

# Waits until something listens on the port.
listening() {
    for _ in $(seq 100); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then return 0; fi
        sleep 0.1
    done
    echo "nothing listens on port $1" >&2
    return 1
}

failed=0
passed=0
# check NAME COMMAND...: runs the command, quietly unless it fails.
check() {
    local name="$1" out
    shift
    if out="$(timeout 120 "$@" 2>&1)"; then
        passed=$((passed + 1))
        echo "ok      $name"
    else
        failed=$((failed + 1))
        echo "FAILED  $name"
        echo "$out" | sed 's/^/        /'
    fi
}

port="$base_port"
for driver in $drivers; do
    giong_port=$((port)); giong_gzip_port=$((port + 1))
    go_port=$((port + 2)); go_gzip_port=$((port + 3))
    port=$((port + 4))
    GIONG_DRIVER="$driver" "$giong_server" --port="$giong_port" > /dev/null &
    pids+=($!)
    GIONG_DRIVER="$driver" "$giong_server" --port="$giong_gzip_port" --compress > /dev/null &
    pids+=($!)
    "$go_server" --port="$go_port" --use_tls=false 2> /dev/null &
    pids+=($!)
    "$bin/gzip-peer" -mode=server -port="$go_gzip_port" 2> /dev/null &
    pids+=($!)
    for p in "$giong_port" "$giong_gzip_port" "$go_port" "$go_gzip_port"; do listening "$p"; done

    echo "== $driver: grpc-go client, giong-grpc server"
    for case in $go_cases; do
        check "$case" "$go_client" --use_tls=false --server_host=127.0.0.1 \
            --server_port="$giong_port" --test_case="$case"
    done

    echo "== $driver: giong-grpc client, grpc-go server"
    for case in $go_cases; do
        check "$case" env GIONG_DRIVER="$driver" "$giong_client" \
            --server_port="$go_port" --test_case="$case"
    done

    echo "== $driver: gzip with grpc-go"
    check "grpc-go client (gzip requests), giong-grpc server" \
        "$bin/gzip-peer" -mode=client -port="$giong_port"
    check "grpc-go client (gzip both ways), giong-grpc server --compress" \
        "$bin/gzip-peer" -mode=client -port="$giong_gzip_port"
    check "giong-grpc client (gzip requests), grpc-go server" \
        env GIONG_DRIVER="$driver" "$giong_client" --server_port="$go_gzip_port" \
        --test_case=gzip_unary

    echo "== $driver: giong-grpc client, giong-grpc server"
    check "all eighteen cases" env GIONG_DRIVER="$driver" "$giong_client" \
        --server_port="$giong_port" --test_case=all
    check "all eighteen cases, server --compress" env GIONG_DRIVER="$driver" "$giong_client" \
        --server_port="$giong_gzip_port" --test_case=all

    echo "== $driver: grpcurl through reflection"
    target="127.0.0.1:$giong_port"
    check "list has the services" bash -c "'$grpcurl' -plaintext $target list \
        | tr '\n' ' ' | grep -q 'grpc.health.v1.Health grpc.reflection.v1.ServerReflection grpc.reflection.v1alpha.ServerReflection grpc.testing.TestService'"
    check "describe a message" bash -c "'$grpcurl' -plaintext $target describe grpc.testing.SimpleRequest \
        | grep -q 'int32 response_size = 2;'"
    check "a call built from reflection" bash -c "'$grpcurl' -plaintext -d '{\"response_size\": 3}' \
        $target grpc.testing.TestService/UnaryCall | grep -q '\"body\": \"AAAA\"'"
    check "health of a known service" bash -c "'$grpcurl' -plaintext \
        -d '{\"service\": \"grpc.testing.TestService\"}' $target grpc.health.v1.Health/Check \
        | grep -q '\"status\": \"SERVING\"'"
    check "health of an unknown service is NOT_FOUND" bash -c "'$grpcurl' -plaintext \
        -d '{\"service\": \"nope\"}' $target grpc.health.v1.Health/Check 2>&1 | grep -q NotFound"

    cleanup
    pids=()
done

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
