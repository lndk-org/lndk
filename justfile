default:
  @just --choose

clean:
  cargo clean

fmt:
  cargo fmt -- --config unstable_features=true --config wrap_comments=true --config comment_width=100

lib-test:
  cargo test --lib

cli-test:
  cargo test --bin lndk-cli

clippy:
  cargo clippy

itest:
  #!/usr/bin/env bash
  TMP_DIR=${TMPDIR:-/tmp}
  git submodule update --init --recursive
  cd lnd/cmd/lnd; go build -tags="peersrpc signrpc walletrpc dev" -o $TMP_DIR/lndk-tests/bin/lnd-itest
  RUSTFLAGS="--cfg itest" cargo test --features itest --test '*' -- --test-threads=1 --nocapture

test:
  @just lib-test
  @just cli-test

test-all:
  @just lib-test
  @just cli-test
  @just itest

fetch-external-protos:  
  #!/usr/bin/env bash
  EXTERNAL_DIR="proto/external"  
  mkdir -p "${EXTERNAL_DIR}/google/rpc"
  
  curl -sSfL -o "${EXTERNAL_DIR}/google/rpc/error_details.proto" "https://raw.githubusercontent.com/googleapis/googleapis/master/google/rpc/error_details.proto"
