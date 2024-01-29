# LNDK-CLI

For now, you can use `lndk-cli` separately from `lndk` meaning you don't need to have a `lndk` binary running for `lndk-cli` to work.

## Installation

To use the `lndk-cli` to pay a BOLT 12 offer, follow these instructions:
- To install lndk-cli: 
	`cargo install --bin=lndk-cli --path .`
- With the above command, `lndk-cli` will be installed to `~/.cargo/bin`. So make sure `~/.cargo/bin` is on your PATH so we can properly run `lndk-cli`.
- Run `lndk-cli -h` to make sure it's working. You'll see output similar to:

```
A cli for interacting with lndk

Usage: lndk-cli [OPTIONS] <COMMAND>

Commands:
  decode     Decodes a bech32-encoded offer string into a BOLT 12 offer
  pay-offer  PayOffer pays a BOLT 12 offer, provided as a 'lno'-prefaced offer string
  help       Print this message or the help of the given subcommand(s)

Options:
  -n, --network <NETWORK>    Global variables [default: regtest]
  -t, --tls-cert <TLS_CERT>  [default: /$HOME/.lnd/tls.cert]
  -m, --macaroon <MACAROON>  [default: /$HOME/.lnd/data/chain/bitcoin/regtest/admin.macaroon]
  -a, --address <ADDRESS>    [default: https://localhost:10009]
  -h, --help                 Print help
```

## Commands 

Once `lndk-cli` is installed, you can use it to pay an offer.

Since `lndk-cli` needs to connect to lnd, you'll need to provide lnd credentials to the binary. If your credentials are not in the default location, you'll need to specify them manually.

If your credentials are in the default location, paying an offer looks like:

`lndk-cli pay-offer <OFFER_STRING> <AMOUNT_MSATS>`

If your credentials are not in the default location, an example command looks like:

`lndk-cli -- --network=mainnet --tls-cert=/credentials/tls.cert --macaroon=/credentials/custom.macaroon --address=https://localhost:10019 pay-offer <OFFER_STRING> <AMOUNT_MSATS>`
