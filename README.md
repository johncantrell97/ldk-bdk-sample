# bdk-ldk-sample
A sample lightning node implementation using BDK and LDK.

## Installation
```
git clone https://github.com/johncantrell97/ldk-bdk-sample
```

## Usage
```
cd bdk-ldk-sample
cargo run <electrum-url> <ldk_storage_directory_path> [<ldk-peer-listening-port>] [bitcoin-network] [announced-node-name announced-listen-addr]
```
`bitcoin-network`: defaults to `testnet`. Options: `testnet`, `regtest`, and `signet`.

`ldk-peer-listening-port`: defaults to 9735.

`announced-listen-addr` and `announced-node-name`: default to nothing, disabling any public announcements of this node.
`announced-listen-addr` can be set to an IPv4 or IPv6 address to announce that as a publicly-connectable address for this node.
`announced-node-name` can be any string up to 32 bytes in length, representing this node's alias.

## Notes

This currently depends on a branch of bdk that adds the required functionality to ElectrumBlockchain and EsploraBlockchain.  You can track the PR here https://github.com/bitcoindevkit/bdk/pull/490 .

Most of the actual bdk-ldk integration is done in a separate library maintained here https://github.com/johncantrell97/bdk-ldk . This will get published as a crate soon as the above PR is merged.  For now it must be used either from source or from git.

## License

Licensed under either:

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
