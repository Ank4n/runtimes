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

/// One proxy delegation of a migrated delegator.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableProxyDelegate<AccountId> {
	/// The account which may act on behalf of the delegator; already translated by the sender.
	pub delegate: AccountId,
	pub proxy_type: PortableProxyType,
	/// The number of blocks that an announcement must be in place for before the corresponding
	/// call may be dispatched. In relay-chain blocks; the receiving chain converts it to its own
	/// block time.
	pub delay: u32,
}

/// Proxy delegations of one delegator, in portable format.
///
/// The deposit does not travel with the delegations: the accounts stage moves it as a
/// `ProxyDeposit` hold, which the receiving chain resizes to its own rates when the delegations
/// arrive.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableProxy<AccountId> {
	/// The account that is delegating to their proxies; already translated by the sender.
	pub delegator: AccountId,
	/// The proxies that were delegated to and that can act on behalf of the delegator. Bounded
	/// by the relay chain's `MaxProxies`.
	pub delegates: BoundedVec<PortableProxyDelegate<AccountId>, ConstU32<32>>,
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
