#!/bin/bash
set -e

create_wallet() {
  echo "Waiting for bitcoind to start..."
  until bitcoin-cli -regtest -rpcuser=user -rpcpassword=pass getblockchaininfo > /dev/null 2>&1
  do
    echo "Waiting for bitcoind..."
    sleep 1
  done
  echo "bitcoind started"

  echo "Creating or loading wallet..."
  bitcoin-cli -regtest -rpcuser=user -rpcpassword=pass createwallet default 2>/dev/null || \
  bitcoin-cli -regtest -rpcuser=user -rpcpassword=pass loadwallet default 2>/dev/null || true
  echo "Wallet ready"
}

create_wallet &

echo "Starting bitcoind in foreground mode..."
bitcoind -conf=/bitcoin/bitcoin.conf -printtoconsole
