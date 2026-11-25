Expiry payouts
==============

An Ark server in expiry payout mode pays the VTXOs of an expired tree to their
owners on-chain. Without expiry payouts, the server takes the bitcoin of a VTXO
that its owner did not refresh or exit before the tree expired.

For every VTXO, the server records an owner mailbox key `M` when it co-signs the
VTXO. When the tree expires, the watchman spends the tree and pays each owner to
`tr(M)`, the BIP-86 Taproot key-path output of `M`, minus a payout fee.


# Enable expiry payouts

Set `expiry_payouts = true` in the captaind configuration before you create the
server database. The mode is fixed when the database is created: a database
created without expiry payouts stays in upstream mode.

For every owner-held VTXO, PostgreSQL requires an owner mailbox binding before
commit and prevents changes to stored bindings. This checks that a binding
exists, not that it identifies the right owner. The check reads the `policy`
column, not the serialized VTXO; it cannot detect a policy column that
disagrees with the serialized coin. A database owner can disable the triggers,
so database access remains trusted. Before a board is registered, its funding
output uses `board_exit.owner_mailbox` and is outside this VTXO check.

Run watchmand against the same database. `expiry_payout_max_fee_rate` in the
watchmand configuration sets the highest feerate of a payout transaction and of
its replacements. The watchman pays the miner fee from the value that the
operator owns in the expired tree. If that is not enough, it adds confirmed
inputs of the watchman wallet.

The watchman pays each root of an expired tree with one transaction. A root is
an on-chain output that funds VTXOs, such as the output of a round or a board,
or an output of the tree that an exit confirmed. A spend that is not resolved
holds the payout of every VTXO under its root, so in a tree that nobody exited
it holds the whole tree. The watchman pays the root when the spend resolves.


# Operator sweep destination

Set one top-level option in `watchmand.toml`. With `sweep_address`, the
operator receives every sweep at that address. With `sweep_descriptor`, the
watchman derives successive operator addresses. Two transactions built at
the same time can pay the same address and both proceed. This setting also
applies to operator change on an expiry payout. Each owner still receives
their payout at `tr(M)`.

Export a public receive descriptor for the server's network from a wallet
whose spending keys you control. Use a fresh, dedicated branch with an
unhardened wildcard, for example `wpkh(<BIP84-account-xpub>/0/*)` for a
Wasabi native SegWit account. Compare the first address with one derived by
the receiving wallet before enabling sweeps. Reserve that branch for watchmand
and use a separate branch for addresses you issue in the wallet app. To switch
an existing server, replace `sweep_address` with `sweep_descriptor` and restart
watchmand. Keep the keys for the old destination. Pending transactions can
still confirm with their original outputs.

The database counts derivation indices from zero. To recover the receiving
wallet, read `SELECT next_index FROM watchman_sweep_index` and scan each
descriptor through index `next_index - 1` if the counter is positive.
Skipped indices can exceed the wallet's usual discovery gap. A change of
descriptor continues at the current index, so extend the receiving wallet's
scan range when you change branches. Keep a backup of the descriptors,
spending keys, and latest database. If you restore a stale database, switch
to a fresh branch before restarting watchmand to prevent address reuse.


# The fee rule

The payout fee of an owner is flat: the payout weight multiplied by the
feerate of the watchman. The server charges it once for each owner in each
expired tree, whatever the number or the value of the VTXOs of the owner.

The watchman reads the feerate when it decides the tree. It is the regular
feerate of its fee estimator, the feerate that it builds the payout
transaction with. The fee is fixed from then on. When bitcoind has no fee
estimate, the fee estimator uses `fallback_fee_rate_regular`, so that rate
then sets the fee.

Set the payout weight in both configurations, to the same value. The default
is 200 vB.

- `watchman.expiry_payout_weight_vb` in the watchmand configuration sets the
  weight that the watchman charges.
- `[fees.expiry_payout] weight_vb` in the captaind configuration sets the
  weight that the server publishes with its other fees. A server without
  expiry payouts refuses this section.

The weight does not have to match the size of the payout transaction. When
the fees of the owners and the value that the operator has in the tree do not
cover the mining fee, the watchman adds confirmed inputs of its wallet.

A wallet reads the weight from the fee schedule of the server:
`ArkInfo::fees.expiry_payout` in the SDK, and `fees.expiry_payout.weight_vb`
of `GET /api/v1/wallet/ark-info` in barkd. `ExpiryPayoutFees::calculate`
multiplies it by a feerate. The feerate of the wallet is not the feerate of
the watchman, so the result is an estimate.

If the payout after the fee is below the dust limit, the server pays nothing and
reports the VTXOs as forfeited. The operator keeps that value.

The payout outputs of the owners do not change when the watchman replaces a
payout transaction to raise its feerate.


# The protocol version

A server in expiry payout mode accepts only the Scuttle protocol version,
`PROTOCOL_VERSION_SCUTTLE_EXPIRY_PAYOUT` in `server-rpc/src/pver.rs`. It
refuses other versions with the error for a client that is too old. A server
without expiry payouts uses the upstream protocol versions.

The wallet is in expiry payout mode when it negotiates the Scuttle protocol
version with the server. In that mode, the wallet sends its owner mailbox in
every request that makes a VTXO: board, arkoor, round and Lightning requests.


# The wallet

The wallet derives its mailbox keypair from the seed, at `m/350'/1'`. An app can
supply another mailbox keypair with `WalletSeed::with_mailbox_keypair`, for
example the key of an on-chain wallet of the app. You must supply the same
keypair on every open. The wallet stores the public key when it first connects
to a server in expiry payout mode, and from then on refuses to open with another
mailbox keypair.

Set `Config::require_expiry_payouts` to open the wallet only with a server in
expiry payout mode.

An arkoor address carries a blinded mailbox. The server unblinds it with the
key of the output and records the result as the owner mailbox of the output.
The sender chooses the address, so the address alone does not prove who owns
the mailbox. A wallet credits a received VTXO only when the server reports it
spendable and bound to the mailbox of the wallet. A received VTXO that the
server does not report so yet stays locked until it does.

The bitcoin of a payout arrives at `tr(M)`, not in the Ark balance. Watch that
output with an on-chain wallet. `Wallet::payout_script` returns it.


# What the wallet reports

`Wallet::sync` asks the server about the expired VTXOs that the wallet still
holds. For a VTXO with an expiry payout, the wallet marks the VTXO spent and
records the payout state:

- `pending`: the payout transaction is less than six blocks deep.
- `paid`: the payout transaction is at least six blocks deep.
- `forfeited`: the fee is not less than the VTXO amount, so the server pays
  nothing.

`WalletVtxo::expiry_payout_state` holds the state. `Wallet::expiry_payout`
returns the state, the VTXO amount, the fee, the payout amount and the payout
address. The REST API returns the state in the optional `expiry_payout` field of
`GET /api/v1/wallet/vtxos/{id}` and the VTXO list.

This is what the server reports. Confirm the payout output on-chain before you
count it as received.

A round, offboard or Lightning action that holds a VTXO ends when the server
pays out that VTXO. A server in expiry payout mode does not register an arkoor
output after its expiry. It refunds the arkoor to the sender on-chain instead.
