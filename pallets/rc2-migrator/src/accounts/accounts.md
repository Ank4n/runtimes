# Account migration

Move all the tokens at this stage. Every account is withdrawn and split by destination: reserves go to the Coretime chain as holds, free balance is teleported to Asset Hub.

The migration ends with zero DOT on the Relay Chain. A few accounts are not withdrawn here because a later stage handles them; they are listed below with that stage.

## What is on the Relay Chain

Each row is one case this stage handles. The rule is filled in as the case is implemented, with the unit test that pins it.

### Balances

| # | Case | Rule |
|---|------|------|
| 1 | Free balance | |
| 2 | Registrar deposit, reserved on the para manager (`paras_registrar::Paras`). A record can say more than the manager has reserved. | |
| 3 | HRMP channel deposit, reserved on the para sovereign (`hrmp::HrmpChannels`) | |
| 4 | HRMP open-channel request deposit, reserved on the sender and, once accepted, the recipient (`hrmp::HrmpOpenChannelRequests`) | |
| 5 | Proxy deposit, reserved on the delegator; a pure proxy's deposit is reserved on the account that created it (`pallet_proxy::Proxies`) | |
| 6 | Proxy announcement deposit, reserved on the announcer (`pallet_proxy::Announcements`) | |
| 7 | Multisig operation deposit, reserved on the depositor (`pallet_multisig::Multisigs`) | |
| 8 | Reserved balance that no deposit record explains | |
| 9 | Named holds: preimage deposits, and leftovers from pallets that left the Relay Chain in AHM v1 (`DelegatedStaking`) | |
| 10 | Locks and freezes | |

### Accounts with a rule of their own

| # | Case | Rule |
|---|------|------|
| 11 | Para sovereign accounts, `para…` | |
| 12 | Sovereign accounts of system paras | |
| 13 | Sibling-format accounts, `sibl…` | |
| 14 | Pure proxies: nonce 0 and at least one `Any` proxy | |
| 15 | Accounts another pallet references through a consumer reference (validators' session keys) | |
| 16 | Module accounts, `modl…` | Not withdrawn here. Sweep empties the pots. |
| 17 | Accounts below the existential deposit | Not withdrawn here. Sweep Dust reaps them. |
| 18 | The migration manager | Not withdrawn here. It pays for the calls that drive the migration and is reaped at Cool Off. |

Issuance that no account holds is not an account; TI Correction burns it.
