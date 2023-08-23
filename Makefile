# This Makefile is used to compile an LND binary for the integration tests.

GO_BUILD := go build
CARGO_TEST := cargo test
LND_PKG := github.com/lightningnetwork/lnd

TMP_DIR := "/tmp"
UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
        TMP_DIR=${TMPDIR}
endif

itest:
	@$(call print, "Building lnd for itests.")
	git submodule update --init --recursive
	cd lnd/cmd/lnd; $(GO_BUILD) -tags="peersrpc signrpc dev" -o $(TMP_DIR)/lndk-tests/bin/lnd-itest$(EXEC_SUFFIX)
	$(CARGO_TEST) -- -- test '*' --test-threads=1 --nocapture

