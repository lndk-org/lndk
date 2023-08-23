# This Makefile is used to compile an LND binary for the integration tests.

GO_BUILD := go build
CARGO_TEST := cargo test
LND_PKG := github.com/lightningnetwork/lnd

TMPDIR := "/tmp"
UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
        TMPDIR=${TMPDIR}
endif

itest:
	@$(call print, "Building lnd for itests.")
	$(GO_BUILD) -tags="peersrpc signrpc dev" -o $(TMPDIR)/lndk-tests/bin/lnd-itest$(EXEC_SUFFIX) $(LND_PKG)/cmd/lnd
	$(CARGO_TEST) -- -- test '*' --test-threads=1 --nocapture

