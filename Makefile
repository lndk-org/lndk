# This Makefile is used to compile an LND binary for the integration tests.

GO_BUILD := go build
CARGO_TEST := cargo test
CARGO_BUILD := cargo build
LND_PKG := github.com/lightningnetwork/lnd

TMP_DIR := "/tmp"
BIN_DIR := $(TMP_DIR)/lndk-tests/bin

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
        TMP_DIR=${TMPDIR}
endif

itest:
	@echo Building lnd for itests.
	git submodule update --init --recursive
	cd lnd/cmd/lnd; $(GO_BUILD) -tags="peersrpc signrpc walletrpc dev" -o $(BIN_DIR)/lnd-itest$(EXEC_SUFFIX)

	@echo Building lndk-cli for itests.
	# This outputs the lndk-cli binary into $(BINDIR)/debug/
	$(CARGO_BUILD) --bin=lndk-cli --target-dir=$(BIN_DIR)

	$(CARGO_TEST) --test '*' -- --test-threads=1 --nocapture
