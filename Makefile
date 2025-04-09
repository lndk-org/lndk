# This Makefile is used to compile an LND binary for the integration tests.

GO_BUILD := go build
CARGO_TEST := cargo test
LND_PKG := github.com/lightningnetwork/lnd

TMP_DIR := "/tmp"
UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
        TMP_DIR=${TMPDIR}
endif

build-lnd:
	@$(call print, "Building lnd for itests.")
	git submodule update --init --recursive
	cd lnd/cmd/lnd; $(GO_BUILD) -tags="peersrpc signrpc walletrpc dev" -o $(TMP_DIR)/lndk-tests/bin/lnd-itest$(EXEC_SUFFIX)

eclair-itest:
	$(MAKE) build-lnd
	RUSTFLAGS="--cfg eclair_test" $(CARGO_TEST) --test integration_tests_eclair -- --test-threads=1 --nocapture

itest:
	$(MAKE) build-lnd
	$(CARGO_TEST) --test '*' -- --test-threads=1 --nocapture

