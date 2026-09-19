// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Portable ("chain-agnostic") wire types of the AHM v2 migration, plus the few helpers whose
//! behavior both sides of the wire must agree on.
//!
//! These are the payloads exchanged between the relay-chain sender (`pallet-rc2-migrator`) and
//! the receiving chains' migrator pallets. They live in their own crate so that no runtime has
//! to depend on another chain's pallets just to speak the wire format: the sending side encodes
//! from these types, each receiving runtime declares what it can represent via ordinary
//! `From`/`TryFrom` impls on them.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	storage::{transactional::with_transaction_opaque_err, TransactionOutcome},
	traits::ConstU32,
	BoundedVec,
};
use polkadot_parachain_primitives::primitives::{Id as ParaId, Sibling};
use scale_info::TypeInfo;
use sp_runtime::{traits::AccountIdConversion, AccountId32};

/// Run `f` inside a storage transaction: `Ok` commits, `Err` rolls every write back.
///
/// This is the rollback primitive of the whole migration pipeline — per-item and per-block
/// isolation on both sides use it, so a failure can never leave state half-written.
pub fn with_rollback<R, E>(f: impl FnOnce() -> Result<R, E>) -> Result<R, E> {
	with_transaction_opaque_err(|| match f() {
		Ok(r) => TransactionOutcome::Commit(Ok(r)),
		Err(e) => TransactionOutcome::Rollback(Err(e)),
	})
	.expect("Layer limit is never reached with per-block nesting; qed")
}

/// The sibling-sovereign account of a para: where a (child) para sovereign's balances continue on
/// a parachain.
///
/// Part of the wire contract: the relay chain sends deposits to this account and the receiving
/// chain looks for them on it, so both sides must derive it identically.
pub fn sibling_account<AccountId>(para_id: u32) -> AccountId
where
	Sibling: AccountIdConversion<AccountId>,
{
	Sibling::from(ParaId::from(para_id)).into_account_truncating()
}

/// The account id under which `who`'s balances and records continue on the destination chains.
///
/// A child para sovereign (`para…`) becomes the sibling sovereign (`sibl…`) — the same parachain
/// as seen from a sibling chain; everyone else keeps their address. Part of the wire contract for
/// the same reason as [`sibling_account`]: the accounts stage moves the *money* to the translated
/// address, so any stage that ships a record under the raw relay-chain key splits that record from
/// its backing. Every account-typed field that leaves the relay chain must come through here.
pub fn translate_destination(who: &AccountId32) -> AccountId32 {
	match ParaId::try_from_account(who) {
		Some(para_id) => sibling_account(para_id.into()),
		None => who.clone(),
	}
}

/// Account balance payload in portable format.
///
/// The relay chain withdraws an account into this shape and the receiving chain integrates it
/// through its regular fungible APIs, so refcounts and events are indistinguishable from locally
/// created state.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableAccount<AccountId, Balance> {
	/// The account address on the receiving chain; already translated by the sender.
	pub who: AccountId,
	/// Balance that stays liquid on the receiving chain.
	pub free: Balance,
	/// Balance that was not liquid on the relay chain; re-established as holds on the receiving
	/// chain, one per entry, translated via `From<PortableHoldReason>`.
	pub holds: BoundedVec<PortableHold<Balance>, ConstU32<5>>,
}

/// One non-liquid part of a migrated account's balance.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableHold<Balance> {
	pub reason: PortableHoldReason,
	pub amount: Balance,
}

/// Chain-agnostic identity of balance that was not liquid on the relay chain.
///
/// This enum is the wire-level contract for hold translation: the relay-chain migrator classifies
/// every non-liquid part of an account into one of these variants, and each receiving runtime
/// declares what the variant becomes locally by implementing `From<PortableHoldReason>` for its
/// `RuntimeHoldReason`. The mapping is therefore an explicit `match` per runtime, with no
/// pallet-index coupling on the wire.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum PortableHoldReason {
	/// Reserved on the relay chain without a named reason, via the old `Currency` API — how all
	/// relay-chain deposits are placed. Carries the registrar and HRMP deposits; attribution to
	/// the pallet owning the deposit happens when that pallet's own state migrates.
	#[codec(index = 0)]
	UnnamedReserve,
	/// A proxy deposit of a delegator whose (portable) definitions travel to the destination.
	/// Resized there when the definitions arrive: the destination reserves its own rates for the
	/// recreated entry and releases the rest as free balance.
	#[codec(index = 1)]
	ProxyDeposit,
	/// Reserve that no pallet's deposit records account for (a known on-chain anomaly). Nothing
	/// stays behind on the relay chain, so it travels under its own reason and stays parked at
	/// the destination for investigation; no stage ever re-attributes it.
	#[codec(index = 2)]
	UnattributedReserve,
}

/// Relay-chain proxy permission in portable format.
///
/// Deliberately carries ONLY the permissions the destination represents: the relay side filters
/// before sending, so untranslatable proxy types (Staking, Governance, …) never travel — their
/// definitions stay on the relay chain.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum PortableProxyType {
	#[codec(index = 0)]
	Any,
	#[codec(index = 1)]
	NonTransfer,
	#[codec(index = 2)]
	CancelProxy,
	#[codec(index = 3)]
	ParaRegistration,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sibling_account_derivation_is_the_wire_contract() {
		// The relay chain sends deposits to this address and the receiving chain looks for them
		// on it. Pinned to raw bytes so a change in either derivation crate shows up here.
		let sov: AccountId32 = sibling_account(2000);
		let bytes: &[u8] = sov.as_ref();
		assert!(bytes.starts_with(b"sibl"));
		assert_eq!(bytes[4..8], 2000u32.to_le_bytes());
		assert!(bytes[8..].iter().all(|b| *b == 0));
	}

	#[test]
	fn translate_destination_rewrites_only_child_sovereigns() {
		let child: AccountId32 = ParaId::from(2000).into_account_truncating();
		assert_eq!(translate_destination(&child), sibling_account::<AccountId32>(2000));

		// A regular account keeps its address.
		let alice = AccountId32::new([1u8; 32]);
		assert_eq!(translate_destination(&alice), alice);

		// A sibling-format sovereign already is the destination address.
		let sibling: AccountId32 = sibling_account(2000);
		assert_eq!(translate_destination(&sibling), sibling);
	}

	#[test]
	fn with_rollback_commits_ok_and_rolls_back_err() {
		sp_io::TestExternalities::default().execute_with(|| {
			let key = b"test:key";

			let r: Result<(), ()> = with_rollback(|| {
				frame_support::storage::unhashed::put(key, &1u32);
				Ok(())
			});
			assert_eq!(r, Ok(()));
			assert_eq!(frame_support::storage::unhashed::get::<u32>(key), Some(1));

			let r: Result<(), ()> = with_rollback(|| {
				frame_support::storage::unhashed::put(key, &2u32);
				Err(())
			});
			assert_eq!(r, Err(()));
			assert_eq!(
				frame_support::storage::unhashed::get::<u32>(key),
				Some(1),
				"an Err must roll every write back"
			);

			// Nesting: an inner commit is still undone by an outer rollback — the per-account /
			// per-block isolation the migrators stack on top of each other.
			let r: Result<(), ()> = with_rollback(|| {
				let inner: Result<(), ()> = with_rollback(|| {
					frame_support::storage::unhashed::put(key, &3u32);
					Ok(())
				});
				assert_eq!(inner, Ok(()));
				Err(())
			});
			assert_eq!(r, Err(()));
			assert_eq!(frame_support::storage::unhashed::get::<u32>(key), Some(1));
		});
	}
}
