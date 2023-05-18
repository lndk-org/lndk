# Changelog

All notable changes to this project will be documented in this file.

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


