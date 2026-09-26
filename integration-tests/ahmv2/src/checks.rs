// Copyright (C) Polkadot Fellows.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot. If not, see <http://www.gnu.org/licenses/>.

//! Pre- and post-migration checks, in the shape of AHM v1's `RcMigrationCheck` and
//! `AhMigrationCheck`.
//!
//! Every check runs on one chain. `pre_check` records the state the migration is about to move,
//! the migration runs, and `post_check` asserts on the outcome against that record. The Relay
//! Chain's record is handed to the checks on the other chains, so they can assert on what arrived.
//! Every check runs inside `hypothetically!` and cannot change the state it inspects.

use crate::mock::network;
use frame_support::hypothetically;
use migrator_types::{translate_destination, PortableProxyType};
use sp_runtime::AccountId32;
use std::collections::BTreeMap;

type Rc = network::relay::Runtime;
type Ct = network::ct::Runtime;
type Ah = network::ah::Runtime;

/// Checks run on the Relay Chain before and after the migration.
pub trait RcMigrationCheck {
	/// Relay Chain payload which is exported for migration checks.
	type RcPrePayload: Clone;

	/// Record the state the migration is about to move.
	fn pre_check() -> Self::RcPrePayload;

	/// Check that the recorded state has left the Relay Chain as expected.
	fn post_check(rc_pre_payload: Self::RcPrePayload);
}

/// Checks run on the Coretime chain before and after the migration.
pub trait CtMigrationCheck {
	/// Relay Chain payload which is exported for migration checks.
	type RcPrePayload: Clone;
	/// Coretime chain payload for the state the migration changes.
	type CtPrePayload: Clone;

	/// Record the Coretime state the migration will change.
	fn pre_check(rc_pre_payload: Self::RcPrePayload) -> Self::CtPrePayload;

	/// Check that what the Relay Chain sent arrived as expected.
	fn post_check(rc_pre_payload: Self::RcPrePayload, ct_pre_payload: Self::CtPrePayload);
}

/// Checks run on Asset Hub before and after the migration.
pub trait AhMigrationCheck {
	/// Relay Chain payload which is exported for migration checks.
	type RcPrePayload: Clone;
	/// Asset Hub payload for the state the migration changes.
	type AhPrePayload: Clone;

	/// Record the Asset Hub state the migration will change.
	fn pre_check(rc_pre_payload: Self::RcPrePayload) -> Self::AhPrePayload;

	/// Check that what the Relay Chain teleported arrived as expected.
	fn post_check(rc_pre_payload: Self::RcPrePayload, ah_pre_payload: Self::AhPrePayload);
}

/// Wrapper for the `frame_support::hypothetically` macro since we want to use it in a macro again.
fn hypothetical_fn<R>(f: impl FnOnce() -> R) -> R {
	hypothetically! { f() }
}

#[allow(clippy::unused_unit)]
#[impl_trait_for_tuples::impl_for_tuples(8)]
impl RcMigrationCheck for Tuple {
	for_tuples! { type RcPrePayload = ( #( Tuple::RcPrePayload ),* ); }

	fn pre_check() -> Self::RcPrePayload {
		(for_tuples! { #( hypothetical_fn(Tuple::pre_check) ),* })
	}

	fn post_check(rc_pre_payload: Self::RcPrePayload) {
		(for_tuples! { #( hypothetical_fn(|| Tuple::post_check(rc_pre_payload.Tuple)) ),* });
	}
}

#[allow(clippy::unused_unit)]
#[impl_trait_for_tuples::impl_for_tuples(8)]
impl CtMigrationCheck for Tuple {
	for_tuples! { type RcPrePayload = ( #( Tuple::RcPrePayload ),* ); }
	for_tuples! { type CtPrePayload = ( #( Tuple::CtPrePayload ),* ); }

	fn pre_check(rc_pre_payload: Self::RcPrePayload) -> Self::CtPrePayload {
		(for_tuples! { #( hypothetical_fn(|| Tuple::pre_check(rc_pre_payload.Tuple)) ),* })
	}

	fn post_check(rc_pre_payload: Self::RcPrePayload, ct_pre_payload: Self::CtPrePayload) {
		(for_tuples! { #(
			hypothetical_fn(|| Tuple::post_check(rc_pre_payload.Tuple, ct_pre_payload.Tuple))
		),* });
	}
}

#[allow(clippy::unused_unit)]
#[impl_trait_for_tuples::impl_for_tuples(8)]
impl AhMigrationCheck for Tuple {
	for_tuples! { type RcPrePayload = ( #( Tuple::RcPrePayload ),* ); }
	for_tuples! { type AhPrePayload = ( #( Tuple::AhPrePayload ),* ); }

	fn pre_check(rc_pre_payload: Self::RcPrePayload) -> Self::AhPrePayload {
		(for_tuples! { #( hypothetical_fn(|| Tuple::pre_check(rc_pre_payload.Tuple)) ),* })
	}

	fn post_check(rc_pre_payload: Self::RcPrePayload, ah_pre_payload: Self::AhPrePayload) {
		(for_tuples! { #(
			hypothetical_fn(|| Tuple::post_check(rc_pre_payload.Tuple, ah_pre_payload.Tuple))
		),* });
	}
}

/// The migration starts from `Pending` and ends at `MigrationDone` on the Relay Chain and the
/// Coretime chain.
pub struct SanityChecks;

impl RcMigrationCheck for SanityChecks {
	type RcPrePayload = ();

	fn pre_check() -> Self::RcPrePayload {
		assert_eq!(
			pallet_rc2_migrator::RcMigrationStage::<Rc>::get(),
			pallet_rc2_migrator::MigrationStage::Pending
		);
	}

	fn post_check(_: Self::RcPrePayload) {
		assert_eq!(
			pallet_rc2_migrator::RcMigrationStage::<Rc>::get(),
			pallet_rc2_migrator::MigrationStage::MigrationDone
		);
	}
}

impl CtMigrationCheck for SanityChecks {
	type RcPrePayload = ();
	type CtPrePayload = ();

	fn pre_check(_: Self::RcPrePayload) -> Self::CtPrePayload {
		assert_eq!(
			pallet_ct_migrator::CtMigrationStage::<Ct>::get(),
			pallet_ct_migrator::MigrationStage::Pending
		);
	}

	fn post_check(_: Self::RcPrePayload, _: Self::CtPrePayload) {
		assert_eq!(
			pallet_ct_migrator::CtMigrationStage::<Ct>::get(),
			pallet_ct_migrator::MigrationStage::MigrationDone
		);
	}
}

// Asset Hub has no migration stage; the impl keeps the checker tuples the same shape on every
// chain, so the Relay Chain's payload fits them all.
impl AhMigrationCheck for SanityChecks {
	type RcPrePayload = ();
	type AhPrePayload = ();

	fn pre_check(_: Self::RcPrePayload) -> Self::AhPrePayload {}

	fn post_check(_: Self::RcPrePayload, _: Self::AhPrePayload) {}
}

/// One Relay Chain account as the accounts stage found it.
#[derive(Clone, Debug)]
pub struct RcAccount {
	/// Free plus reserved balance.
	pub total: u128,
	/// Reserved balance, holds included.
	pub reserved: u128,
	/// Whether the account is one the migration leaves in place.
	pub stays: bool,
	/// Whether the account never signed and grants an `Any` proxy, so all of it goes to the
	/// Coretime chain.
	pub pure_like: bool,
}

impl RcAccount {
	/// An account whose whole balance is teleported to Asset Hub.
	fn is_plain(&self) -> bool {
		self.reserved == 0 && !self.pure_like
	}
}

/// What the Relay Chain held before the migration.
#[derive(Clone, Debug)]
pub struct RcAccountsPre {
	pub accounts: BTreeMap<AccountId32, RcAccount>,
	pub total_issuance: u128,
}

/// The migrating accounts that land on one destination account.
#[derive(Clone, Debug, Default)]
struct Destination {
	/// Sum of the sources' totals.
	total: u128,
	/// Sum of the sources' reserved balances.
	reserved: u128,
	/// Every source is plain.
	all_plain: bool,
	/// Every source is pure-like.
	all_pure_like: bool,
}

/// Group the migrating accounts by the account they land on, since a child sovereign and a
/// sibling-format account for the same para share a destination.
fn destinations(rc_pre: &RcAccountsPre) -> BTreeMap<AccountId32, Destination> {
	let mut out: BTreeMap<AccountId32, Destination> = BTreeMap::new();
	for (who, account) in rc_pre.accounts.iter().filter(|(_, a)| !a.stays) {
		let dest = out.entry(translate_destination(who)).or_insert(Destination {
			all_plain: true,
			all_pure_like: true,
			..Default::default()
		});
		dest.total += account.total;
		dest.reserved += account.reserved;
		dest.all_plain &= account.is_plain();
		dest.all_pure_like &= account.pure_like;
	}
	out
}

/// How much `who`'s balance grew from `before` to `after`. Panics if it shrank, which no step of
/// the migration may cause.
fn grown(who: &AccountId32, before: u128, after: u128) -> u128 {
	after
		.checked_sub(before)
		.unwrap_or_else(|| panic!("{who:?} shrank: {before} -> {after}"))
}

fn total_on<T>(who: &AccountId32) -> u128
where
	T: frame_system::Config<
		AccountId = AccountId32,
		AccountData = pallet_balances::AccountData<u128>,
	>,
{
	let data = frame_system::Account::<T>::get(who).data;
	data.free + data.reserved
}

/// Every account leaves the Relay Chain whole or stays whole, and each lands where the split rule
/// sends it: plain accounts on Asset Hub, pure-like accounts on the Coretime chain, deposit
/// holders on both.
pub struct AccountsChecker;

impl RcMigrationCheck for AccountsChecker {
	type RcPrePayload = RcAccountsPre;

	fn pre_check() -> Self::RcPrePayload {
		let manager = pallet_rc2_migrator::Manager::<Rc>::get();
		let ed = <Rc as pallet_balances::Config>::ExistentialDeposit::get();

		let accounts = frame_system::Account::<Rc>::iter()
			.map(|(who, info)| {
				let data = info.data;
				let total = data.free + data.reserved;
				// Preimage deposits are released before the first withdrawal, so they do not
				// keep an account here.
				let has_other_holds = pallet_balances::Holds::<Rc>::get(&who)
					.iter()
					.any(|h| !matches!(h.id, network::relay::RuntimeHoldReason::Preimage(_)));
				let is_module = AsRef::<[u8]>::as_ref(&who).starts_with(b"modl");
				let stays = manager.as_ref() == Some(&who) ||
					is_module ||
					total < ed ||
					data.frozen > 0 ||
					has_other_holds;
				let pure_like = info.nonce == 0 &&
					pallet_proxy::Proxies::<Rc>::get(&who).0.iter().any(|def| {
						matches!(def.proxy_type.clone().try_into(), Ok(PortableProxyType::Any))
					});
				(who, RcAccount { total, reserved: data.reserved, stays, pure_like })
			})
			.collect();

		RcAccountsPre { accounts, total_issuance: pallet_balances::TotalIssuance::<Rc>::get() }
	}

	fn post_check(rc_pre: Self::RcPrePayload) {
		let mut migrated = 0u128;
		for (who, account) in &rc_pre.accounts {
			let data = frame_system::Account::<Rc>::get(who).data;
			if account.stays {
				assert_eq!(
					data.free + data.reserved,
					account.total,
					"account {who:?} should have stayed on the Relay Chain untouched"
				);
			} else {
				// Gone, or a zero-balance shell some pallet still references.
				assert_eq!(
					(data.free, data.reserved),
					(0, 0),
					"account {who:?} should have left the Relay Chain whole"
				);
				migrated += account.total;
			}
		}

		// Everything the accounts left with is in the ledger, split by destination, and the
		// issuance fell by exactly that.
		let ledger = pallet_rc2_migrator::RcMigratedBalance::<Rc>::get();
		assert_eq!(ledger.ct_reserved + ledger.ct_free + ledger.ah_free, migrated);
		let issuance = pallet_balances::TotalIssuance::<Rc>::get();
		assert_eq!(issuance, rc_pre.total_issuance - migrated);
		assert_eq!(ledger.kept, issuance);

		// No preimage deposit survives the stage's init.
		assert!(
			pallet_preimage_deposits().is_empty(),
			"preimage deposits left: {:?}",
			pallet_preimage_deposits()
		);
	}
}

/// The preimage requests that still hold a deposit.
fn pallet_preimage_deposits() -> Vec<sp_core::H256> {
	pallet_preimage::RequestStatusFor::<Rc>::iter()
		.filter(|(_, status)| match status {
			pallet_preimage::RequestStatus::Unrequested { .. } => true,
			pallet_preimage::RequestStatus::Requested { maybe_ticket, .. } =>
				maybe_ticket.is_some(),
		})
		.map(|(hash, _)| hash)
		.collect()
}

/// Coretime balances of every destination, and the chain's issuance, before the migration.
#[derive(Clone, Debug)]
pub struct CtAccountsPre {
	pub balances: BTreeMap<AccountId32, (u128, u128)>,
	pub total_issuance: u128,
	pub minted: u128,
}

impl CtMigrationCheck for AccountsChecker {
	type RcPrePayload = RcAccountsPre;
	type CtPrePayload = CtAccountsPre;

	fn pre_check(rc_pre: Self::RcPrePayload) -> Self::CtPrePayload {
		let balances = destinations(&rc_pre)
			.into_keys()
			.map(|dest| {
				let data = frame_system::Account::<Ct>::get(&dest).data;
				(dest, (data.free, data.reserved))
			})
			.collect();
		CtAccountsPre {
			balances,
			total_issuance: pallet_balances::TotalIssuance::<Ct>::get(),
			minted: pallet_ct_migrator::CtMintedTotal::<Ct>::get(),
		}
	}

	fn post_check(rc_pre: Self::RcPrePayload, ct_pre: Self::CtPrePayload) {
		let failed: Vec<_> = pallet_ct_migrator::FailedAccounts::<Ct>::iter_keys().collect();
		assert!(failed.is_empty(), "accounts failed to integrate: {failed:?}");

		let minted = pallet_ct_migrator::CtMintedTotal::<Ct>::get() - ct_pre.minted;
		assert_eq!(
			pallet_balances::TotalIssuance::<Ct>::get(),
			ct_pre.total_issuance + minted,
			"Coretime issuance grew by something other than the migration"
		);

		let mut arrived = 0u128;
		for (dest, expected) in destinations(&rc_pre) {
			let (free_before, reserved_before) = ct_pre.balances[&dest];
			let data = frame_system::Account::<Ct>::get(&dest).data;
			let delta = grown(&dest, free_before + reserved_before, data.free + data.reserved);
			arrived += delta;

			if expected.all_plain {
				assert_eq!(delta, 0, "plain account {dest:?} must not land on Coretime");
			}
			if expected.all_pure_like {
				assert_eq!(delta, expected.total, "pure-like {dest:?} must land on Coretime whole");
			}
			assert!(
				grown(&dest, reserved_before, data.reserved) <= expected.reserved,
				"{dest:?} holds more on Coretime than it reserved on the Relay Chain"
			);
		}
		assert_eq!(arrived, minted, "Coretime minted for accounts that did not migrate");
	}
}

/// Asset Hub balances of every destination, its issuance and its checking account, before the
/// migration.
#[derive(Clone, Debug)]
pub struct AhAccountsPre {
	pub balances: BTreeMap<AccountId32, u128>,
	pub total_issuance: u128,
	pub checking: u128,
}

fn ah_checking_account() -> u128 {
	total_on::<Ah>(&pallet_xcm::Pallet::<Ah>::check_account())
}

impl AhMigrationCheck for AccountsChecker {
	type RcPrePayload = RcAccountsPre;
	type AhPrePayload = AhAccountsPre;

	fn pre_check(rc_pre: Self::RcPrePayload) -> Self::AhPrePayload {
		let balances = destinations(&rc_pre)
			.into_keys()
			.map(|dest| {
				let total = total_on::<Ah>(&dest);
				(dest, total)
			})
			.collect();
		AhAccountsPre {
			balances,
			total_issuance: pallet_balances::TotalIssuance::<Ah>::get(),
			checking: ah_checking_account(),
		}
	}

	fn post_check(rc_pre: Self::RcPrePayload, ah_pre: Self::AhPrePayload) {
		// A teleport in moves balance out of the checking account; it mints nothing.
		assert_eq!(pallet_balances::TotalIssuance::<Ah>::get(), ah_pre.total_issuance);

		let mut arrived = 0u128;
		for (dest, expected) in destinations(&rc_pre) {
			let delta = grown(&dest, ah_pre.balances[&dest], total_on::<Ah>(&dest));
			arrived += delta;

			if expected.all_plain {
				assert_eq!(delta, expected.total, "plain account {dest:?} must land on AH whole");
			}
			if expected.all_pure_like {
				assert_eq!(delta, 0, "pure-like {dest:?} must not land on AH");
			}
		}
		assert_eq!(
			ah_pre.checking - ah_checking_account(),
			arrived,
			"the checking account paid for something other than the migration"
		);
	}
}

/// What the Relay Chain teleported, as Asset Hub's checking account paid it out.
pub fn ah_checking_paid(ah_pre: &AhAccountsPre) -> u128 {
	ah_pre.checking - ah_checking_account()
}
