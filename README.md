# Diamond Lightning Node

`dln-node`, Diamond Lightning Node is designed to be a hyper secure Lightning node, and like a diamond it aims to be transparent and hard, and created to perfection using a controlled process. Design goals.

* Transparent, as in user auditable, under a GPL3 license.
* Hard, as in low attack surface, minimalistic, defensive coding, and functional design.
* Controlled, as in there is a process around how the software is created, checked in, reviewed, verified, delivered and deployed.

Currently it's built around [`ldk-node`], controlled entirely over
Nostr and designed with the ability to run without holding its own keys, and instead having them in an external signer, that can
run in a secure enclave.

It takes its chain data from a Bitcoin Core RPC endpoint, exposes no HTTP or
gRPC surface, and delegates signing to an external signer that can live in
another process — or, for tests, in-process or not at all.

## Control plane

Nostr is the node's only API. There is no CLI and no local socket. Two message
channels carry different classes of operation:

| Channel | Kind | Covers |
|---|---|---|
| **NWC** (NIP-47 shaped) | standard NWC kinds | wallet operations — balance, invoices, payments, on-chain |
| **NCC** | 23198 request / 23199 response, NIP-04 encrypted | node operations — channels, peers, fees, routing, network queries |

On NWC that means `get_info` and `get_balance`; `make_invoice`,
`lookup_invoice`, `list_invoices` and `pay_invoice`; the hold-invoice
primitives `make_hold_invoice`, `settle_hold_invoice` and
`cancel_hold_invoice`; keysend; BOLT12 offers; and the on-chain set
(`pay_onchain`, `make_new_address`, `list_addresses`, `list_transactions`,
fee estimation). On NCC: `open_channel`, `list_channels`, `close_channel`,
`connect_peer`, `disconnect_peer`, `list_peers`, `get_channel_fees`,
`set_channel_fees`, `get_forwarding_history`, `query_routes`, the network
queries, and `subscribe_notifications`.

**Not all of the NWC surface is NIP-47.** Hold invoices in particular are not
in the specification; the node depends on a fork of `nostr-sdk`/`nwc` that
defines them. The dispatch in `src/lib.rs` is the authoritative list.

Both channels are gated by **grants**: kind-30078 events addressed with a `d`
tag of `{service_pubkey}:{client_pubkey}`. A grant carries an optional
client-wide `quota`, a **`methods`** map for NWC and a **`control`** map for
NCC, each entry holding a per-method `access_rate`. Authorisation is per
method *and* per client key, so different clients can hold different
capabilities against the same node. A method absent from the grant is refused
with `Restricted`.

## Signing

On the default paths the node never sees raw keys. LDK's `KeysInterface` is
backed by a signing client, so every signing operation is a request the signer
validates against policy before honouring — and can refuse.

`signer.transport` selects how:

| Mode | Where keys live | Policy validation | Use |
|---|---|---|---|
| `nostr` | separate signer process, reached over a relay | full | production |
| `embedded` | in-process signer, no transport | two routing-balance policies downgraded to warnings | tests |
| `none` | the node itself, via ldk-node's own `KeysManager` | none | bring-up only |

`embedded` also runs without a state persister, so signer state does not
survive a restart.

`none` bypasses the signer entirely. It exists because two nodes running
embedded signers derive the same `node_id` and therefore cannot peer with each
other, which makes multi-node scenarios impossible. **It is not a production
configuration**: the node holds its own keys, and nothing validates what it
signs.

## Configuration

`config.toml`, read from the working directory:

```toml
[node]
network = "regtest"          # regtest | testnet | signet | bitcoin
listening_port = 9735
data_dir = "./data"
# alias = "optional"

[nostr]
relay = "ws://localhost:7777"
private_key = "<nsec or hex>"

[wallet]
max_channel_size_sats = 10000000
min_channel_size_sats = 20000
auto_accept_channels = false

[bitcoind]
rpc_host = "127.0.0.1"
rpc_port = 18443
rpc_user = "rpcuser"
rpc_password = "rpcpass"

[signer]
transport = "nostr"          # nostr | embedded | none
relay = "ws://localhost:7777"   # nostr transport only
nsec = "<node proxy nsec hex>"  # nostr transport only
signer_pubkey = "<signer pubkey hex>"  # nostr transport only
```

`[bitcoind]` and `[signer]` are optional. Omitting `[bitcoind]` starts the NWC
service without a Lightning node attached. **Omitting `[signer]` selects
`embedded`** — an in-process signer with two routing-balance policies
downgraded to warnings, which is a test configuration rather than a default
worth inheriting by accident.

## Building

```
cargo build --bin dln-node
```

## Testing

End-to-end scenarios live in [`dln-node-e2e`], which drives real nodes against
a Bitcoin Core regtest container:

- `two_dln_nodes` — two nodes, channel open, invoice, Lightning payment
- `onchain_payment` — on-chain send, verified against bitcoind
- `hold_invoice` — the settle and cancel paths of a held HTLC

They run with `transport = "none"`, because two nodes with embedded signers
derive the same `node_id` and cannot peer.

Scenarios for the BTK chain are in [`dln-node-knots-e2e`], and the exchange
built on this node is tested in [`diamond-x-e2e`], which also carries the
harness all three share.

[`dln-node-e2e`]: https://github.com/DarkWebDivingClub/dln-node-e2e
[`dln-node-knots-e2e`]: https://github.com/DarkWebDivingClub/dln-node-knots-e2e
[`diamond-x-e2e`]: https://github.com/DarkWebDivingClub/diamond-x-e2e

[`ldk-node`]: https://github.com/lightningdevkit/ldk-node
