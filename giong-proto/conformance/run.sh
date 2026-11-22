#!/usr/bin/env bash
# Runs Google's protobuf conformance suite against giong-proto.
#
#   giong-proto/conformance/run.sh
#
# Builds conformance_test_runner from the protobuf sources (tag PROTOBUF_TAG,
# default v36.2) into PROTOBUF_DIR (default target/protobuf here) the first
# time, which takes about 10 minutes on 4 cores and needs cmake, ninja and a
# C++ compiler; then builds the testee and runs every test up to proto3 with
# --enforce_recommended. JSON and text-format tests show as skipped:
# giong-proto has the binary format only. Fails if any test fails.
set -euo pipefail
cd "$(dirname "$0")"

TAG=${PROTOBUF_TAG:-v36.2}
SRC=${PROTOBUF_DIR:-$PWD/target/protobuf}
RUNNER=$SRC/build/conformance_test_runner

if [[ ! -x $RUNNER ]]; then
  if [[ ! -d $SRC/.git ]]; then
    git clone --quiet --depth 1 --branch "$TAG" --recurse-submodules --shallow-submodules \
      https://github.com/protocolbuffers/protobuf "$SRC"
  fi
  cmake -S "$SRC" -B "$SRC/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
    -Dprotobuf_BUILD_CONFORMANCE=ON -Dprotobuf_BUILD_TESTS=OFF \
    -Dprotobuf_FORCE_FETCH_DEPENDENCIES=ON -Dprotobuf_BUILD_LIBUPB=OFF >/dev/null
  ninja -C "$SRC/build" conformance_test_runner
fi

cargo build --release --bin testee
"$RUNNER" --enforce_recommended --maximum_edition PROTO3 target/release/testee 2>&1 \
  | grep -v '^WARNING: All log messages\|fork_pipe_runner'
