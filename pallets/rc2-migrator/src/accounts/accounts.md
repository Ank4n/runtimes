# Account Migration

Accounts are migrated with all their balance at the start of the data migration. Each account's balance splits by destination: deposits and a small working buffer go to the Coretime chain, everything else is teleported to Asset Hub.

## User Impact

> [!CAUTION]
> All funds are **moved** off the Relay Chain. Only the accounts listed under [Accounts That Stay](#accounts-that-stay) keep a balance here.

- The Account ID stays the same, except for parachain sovereign accounts (see [Sovereign Account Translation](#sovereign-account-translation)).
- Free balance is teleported to the same account on Asset Hub.
- Deposits recorded by the registrar, HRMP and proxy pallets move to the Coretime chain as holds. A working buffer of free balance (`Config::CtFreeBuffer`) follows them, so the owner can pay fees there without a teleport first. Free balance below Asset Hub's existential deposit also follows the deposit instead of being teleported.
- Deposits whose purpose ends with the Relay Chain are released and teleported to Asset Hub as free balance: proxy sets with no permission that has meaning on the Coretime chain, proxy announcements and multisig operations.
- Preimage deposits are released before the first account is withdrawn. The preimages stay on the Relay Chain.
- An account that never signed (`nonce == 0`) and grants an `Any` proxy is treated as a pure proxy. Its delegate keeps control only where the definitions are recreated, so its whole balance goes to the Coretime chain.
- An account that some pallet still references (session keys being the known case) keeps its record on the Relay Chain as a zero-balance shell. Its balance moves like any other account's.

### Accounts That Stay

- The migration manager. It pays for the calls that drive the migration.
- Pallet (module) accounts. The `Sweep` stage empties their pots.
- Accounts below the existential deposit. The `SweepDust` stage handles them.
- Accounts with locks, freezes or named holds. None are expected on the Relay Chain after AHM v1, and translating them is not implemented.
- An account that cannot be withdrawn cleanly is left in place whole and reported with `AccountSkipped`.

## Sovereign Account Translation

A parachain's sovereign account is derived differently on the Relay Chain and on a parachain:

- On the Relay Chain: `"para" ++ para_id ++ 00..`
- On the Coretime chain, Asset Hub and every other sibling: `"sibl" ++ para_id ++ 00..`

Every account matching the Relay Chain pattern is translated to the sibling one before it is sent (`migrator_types::translate_destination`), so the balance lands on the account the para controls on the destination. Sibling-format accounts on the Relay Chain migrate untranslated.

## Reserve Attribution

Every deposit on the Relay Chain is an unnamed reserve, so an account's reserve does not say which deposits it is made of. Before the first withdrawal, `AccountsMigrator::init` indexes `ExpectedReserves` from the deposit fields the owning pallets record:

| Record | Account | Continues as |
| ------ | ------- | ------------ |
| `paras_registrar::Paras` | manager | `RegistrarDeposit` hold on the Coretime chain |
| `hrmp::HrmpChannels` | sender and recipient sovereigns | `HrmpDeposit` hold on the Coretime chain |
| `hrmp::HrmpOpenChannelRequests` | sender sovereign | `HrmpDeposit` hold on the Coretime chain |
| `pallet_proxy::Proxies`, at least one portable permission | delegator | `ProxyDeposit` hold on the Coretime chain |
| `pallet_proxy::Proxies`, no portable permission | delegator | free balance on Asset Hub |
| `pallet_proxy::Announcements` | announcer | free balance on Asset Hub |
| `pallet_multisig::Multisigs` | depositor | free balance on Asset Hub |

The live reserve is consumed in the table's order, so when it covers less than the records, the deposits that continue are made whole first. Reserve beyond every record travels as an `UnattributedReserve` hold on the Coretime chain and is reported with `UnattributedReserve`. A confirmed HRMP open request's recipient deposit is not recorded until the next session boundary, so in that window it is unattributed.

## XCM

Each block withdraws up to `MAX_ACCOUNTS_PER_BLOCK` accounts and sends what it burned, in one storage transaction: a failed send rolls the block's withdrawals back and the block is retried.

- **Coretime chain:** `receive_accounts`, as a root `Transact`, with up to `MAX_ACCOUNTS_PER_XCM` accounts per message. Each account is minted and its holds placed through the fungible APIs. An account that fails is rolled back and parked in `FailedAccounts`; the rest of the batch continues.
- **Asset Hub:** a teleport with up to `MAX_TELEPORTS_PER_XCM` beneficiaries per message: one `ReceiveTeleportedAsset` of the total, then one `DepositAsset` per beneficiary. No receiving pallet is needed.

The Relay Chain does no teleport tracking, so its total issuance falls by everything burned. Asset Hub checks the teleport in against its checking account, so its total issuance does not change.

The Relay Chain emits `Balances::Burned` for every withdrawn account, except for the zero-balance shells: their balance is written directly and only reported with `AccountShellDrained`. Every sent batch is reported with `AccountsBatchSent` or `AccountsTeleported`.

## Balance Tracking

`init` seeds `RcMigratedBalance::kept` with the Relay Chain's total issuance. Each block moves what it burned from `kept` into `ct_reserved`, `ct_free` and `ah_free`, so the ledger sums to the starting issuance after every block. The Coretime chain accrues what it minted in `CtMintedTotal`.

## Provider and Consumer References

Releasing an account's reserve drops the reserve's consumer reference, and the burn then reaps the account. No reference counts travel with the account: the Coretime chain establishes them through the fungible APIs as it mints and holds.
