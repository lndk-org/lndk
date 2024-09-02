# Changelog

All notable changes to this project will be documented in this file.

## [0.2.0] - 2024-09-02

### Documentation

- Add instructions for paying an offer with the cli
- Add instructions for paying an Eclair offer
- Update docs with new mode for passing in creds
- Update docs with server updates
- Update cargo fmt instructions for contributors
- Update compiling instructions
- Add bakemacaroon instructions for paying offers

### Miscellaneous Tasks

- Update cargo-dist and regenerate
- Update release process documentation & remove cosign key

### README

- Add discord invitation
- Fix bakemacaroon typo

### Testing

- Add bitcoind node setup for testing
- Test that ldk node can send onion message
- Add lnd git submodule
- Add initial Makefile for lnd bin
- Organize needed bitcoind data in a struct
- Clean up directory structure
- Add a utility for retrying grpc calls
- Add lnd to integration tests
- Test that lndk forwards onion messages
- Specify log level in ldk nodes
- Split off connect_network fn for reuse
- Split out setup_lndk fn for reuse
- Update test pubkey to return two keys

### Actions

- Update github actions with new test process
- Install protobuf-compiler so we can build tonic_lnd
- Specify Rust version to use
- Update fmt workflow with max comment width
- Update codecov-actions to v4

### Cfg

- Pass in extra ips to tls certificate

### Cli

- Add global arguments for connecting to lnd
- Add cli command to pay offer
- Default macaroon path should depend on the specified network
- Add option to pass cert/macaroon in directly to cli
- Update CLI to connect to new gRPC server
- TLS connection with server
- Add option to pass in LNDK tls cert directly
- Return error code on error
- Split tls/macaroon processing into separate functions
- Add get_invoice/pay_invoice commands
- Don't pass in full cli args to read function

### Clippy

- Move outer attribute into inner
- Move outer attribute into inner

### Config

- Clarify where log file is stored by default

### Itests

- Update lnd submodule to tagged hash change
- Add walletrpc subserver to lnd Makefile/README
- Add lnd API calls needed to set up channels
- Export bitcoind for tests
- Bump ldk-sample to newer version
- Advertise ldk node address
- Set more granular lnd logs
- Add lnd new_address api call

### Lib

- Implement and use OfferMessageHandler on OfferHandler
- Refactor create_invoice_request to be a method of OfferHandler
- When finding route, add missing fee ppm parameter
- Move and Arc-ify OffersHandler
- Remove offer from map if we fail/succeed to pay

### Lnd

- Export network verifier for cli
- Export get_lnd_client, features_support_onion_messages & network checker
- Convert network string to lowercase before processing
- Move get network logic into separate function

### Logs

- Filter out useless dependency logs
- Add ldk sublogger

### Main

- Move main logic for running lndk into a library
- Replace simple logger with log4rs
- Ignore unused imports from configure_me
- Add config option for specifying log level
- Add grpc server config options
- Auto-create data directory at ~/.lndk
- Move logger setup further up
- Handle sigterm/sigint signals

### Main+lib

- Move logger out of run method

### Maintainers

- Add notes from first release

### Messenger

- Fix PeerConnected empty features bug

### Multi

- Propagate shutdown signal from caller to lndk
- Refactor to create OfferHandler and LndkOnionMessenger
- Add verification details to invoice request
- Split off uir signing portion of create_invoice_request into a method
- Upgrade to ldk v20
- Send offer payment
- Add option to pass cert/macaroon in directly to lndk
- Delete started channel
- Expose payment when done tracking it
- Setup grpc server
- Format comments to 100 width
- Separate pay_offer into two methods
- Remove metadata ir parameter
- Allow passing in a payer note
- Configurable timeout for receiving invoice
- Move to derive_new_key for key gen

### Offers

- Rename create_invoice_request
- Add logic for connecting to the introduction node peer
- Validate offer amount user input
- Wait for onion messenger ready signal before sending request
- Build a reply path for invoice request
- Send invoice request
- Verify invoice upon return
- Add InvoicePayer for paying an offer
- Add timeout for invoice response
- Change OfferState to PaymentState
- Return PaymentId for later use
- Split create_invoice_request from send_invoice_request
- Track active payments instead of offers
- Improve offer flow logs
- Handle invoice request build failures more gracefully
- Rename pay_invoice to send_payment
- Change validate_amount parameters
- Remove extra cltv expiry delta
- Add back extra cltv expiry delta
- Don't choose unadvertised node as introduction node

### Onion

- Remove RefCell from MessengerUtilities

### Readme

- Update branch instructions

### Release

- Update cargo-dist

### Server

- Require TLS for interacting with server
- New get_info and pay_offer endpoints
- Return invoice object for now
- Only log request message
- Include payer note in invoice contents
- Correctly convert invoice features

### Server+cli

- Add DecodeInvoice command

### Utils

- Add Default for MessengerUtilities to satisfy clippy

## [0.0.1] - 2023-05-18

### Documentation

- Add contributor covenant code of conduct v2.1
- Fix typo
- Add command for generating a custom macaroon

### Miscellaneous Tasks

- Generate release workflow with `cargo-dist`
- Include Sigstore Cosign signing in release workflow
- Add MAINTAINERS.md with release process
- Add release hook for CHANGELOG generation

### README

- Add architecture description for onion messages
- Link contributor guidelines
- Add more specific instructions for compiling and running LND
- Move architecture section to a separate file
- Split off the cargo-crev note into its own subsection
- Update running lndk instructions to make it more obvious there is a config file option

### Actions

- Include token in codecov action

### Arch

- Update references to org

### Cargo

- Add tonic_lnd dependency
- Add futures crate
- Add tokio with multi threaded runtime
- Update repository
- Set version to 0.0.1

### Contributing

- Add contributor guide
- Point developers to discussions for meta help
- Update references to org repo

### Cosign

- Add github actions pubkey and signature

### Github

- Add initial CI workflow
- Create coverage reports
- Use actions-rs/toolchain@v1; bump checkout action to v3

### Gitignore

- Add target and cargo lock

### Lnd

- Add docs for lnd node signer

### Lnd/docs

- Add documentation for lnd client setup and features

### Lndk

- Cargo new

### Main

- Add blocking lnd client fetch and example call
- Pull argument parsing out + add enum
- Run clippy fix
- Run cargo fmt
- Implement NodeSigner using LND signerrpc
- Implement EntropySource trait for Onion Messenger
- Implement logger trait for messenger utilities
- Add onion messenger
- Advertise onion messaging feature bit upon startup
- Test advertising of onion bit
- Add messenger events and consumer
- Run onion messenger events loop and init with online peers
- Implement peer events producer to supply messenger events
- Implement PeerEventProducer for LND's peer events stream
- Consume peer events from LND's subscription API
- Remove unnecessary info clone
- Small logging fixups
- Exit with error on bad args, don't panic
- Buffer by peers length +1 to prevent panic when we have no peers
- Remove copy trait from MessengerEvents enum
- Add incoming message events to consumer
- Add producer for incoming messages
- Implement IncomingMessageProducer trait for LND's grpc api
- Consume message events from LND's API
- Change name of messages_exit_sender to clarify that it's for incoming messages
- Add local CurrentPeers map to keep up-to-date track of peers to send outgoing onion messages to
- Alter testing pubkey function to generate a random key
- Update logs to match the outlined standards
- Add producer and consumer for processing outgoing onion messages
- Spin up outgoing message producer in a new task
- Send one outgoing message per peer rather than all at once
- Fail if LND does not support onion messages

### Maintainers

- Update cosign key pair generation

### Multi

- Make internal peers map private and surface via method
- Remove onion_support from peer_connected
- Add test utils with deterministic pubkey generation
- Rename current peers to TokenLimiter
- Add rate limiter trait implemented by TokenLimiter
- Add clock module for handling of time, implement with tokio
- Add rate limiting to current peers tracker
- Move lnd client setup and feature checks into lnd module
- Move LndNodeSigner into lnd module
- Move messenger utilities into onion_message module
- Move onion messenger into module

### Multi/refactor

- Move CurrentPeers into its own module

### Onion_messenger

- Fixup and update documentation

### Onion_messenger/docs

- Update MessengerUtilities docs

### Readme

- Update to require dev build tag and custom messaging workaround
- Move architecture to bottom of initial resource list
- Update links to org
- Add high level description of project and milestones
- Update github link to org

### Workflows

- Remove frozen tag from cargo test


