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

//! Accounts stage: withdraws account balances on the relay chain and sends them to the Coretime
//! chain in portable format. Every non-liquid part of a balance is classified into a
//! [`PortableHoldReason`]; the receiving runtime decides what each becomes locally.

use crate::*;
use frame_support::{
	defensive_assert,
	traits::tokens::{Fortitude, Precision, Preservation},
};
use sp_runtime::traits::Zero;

pub type AccountInfoFor<T> = frame_system::AccountInfo<
	<T as frame_system::Config>::Nonce,
	pallet_balances::AccountData<u128>,
>;

/// Account-id prefixes whose translation to the destination chain is not designed yet: parachain
/// sovereign, sibling sovereign and pallet (module) accounts. They are skipped, not migrated.
const UNTRANSLATED_PREFIXES: [&[u8]; 3] = [b"para", b"sibl", b"modl"];

pub struct AccountsMigrator<T>(PhantomData<T>);

impl<T: Config> AccountsMigrator<T> {
	/// Migrate accounts until the per-block limit is reached.
	///
	/// Returns the cursor to continue from on the next block, or `None` once the account space is
	/// exhausted. The caller wraps this in a storage transaction; an `Err` rolls back the whole
	/// block's withdrawals.
	pub fn migrate_many(last_key: Option<T::AccountId>) -> Result<Option<T::AccountId>, Error<T>> {
		let mut iter = match &last_key {
			Some(last_key) => frame_system::Account::<T>::iter_from_key(last_key.clone()),
			None => frame_system::Account::<T>::iter(),
		};

		let mut batch = Vec::new();
		let mut processed = 0u32;
		let maybe_last_key = loop {
			let Some((who, info)) = iter.next() else { break None };
			processed += 1;

			// Each account is withdrawn in its own transaction: a failure rolls back that
			// account only, so it is skipped whole, never half-withdrawn.
			let withdrawn = with_transaction_opaque_err::<_, Error<T>, _>(|| {
				match Self::withdraw_account(&who, info) {
					Ok(ok) => TransactionOutcome::Commit(Ok(ok)),
					Err(e) => TransactionOutcome::Rollback(Err(e)),
				}
			})
			.expect("Always returning Ok; qed");

			match withdrawn {
				Ok(Some(account)) => batch.push(account),
				Ok(None) => (),
				Err(e) => {
					log::warn!(target: LOG_TARGET, "Skipping account {who:?}: {e:?}");
				},
			}

			if batch.len() >= MAX_ACCOUNTS_PER_XCM as usize {
				Pallet::<T>::send_accounts(core::mem::take(&mut batch))?;
			}
			if processed >= MAX_ACCOUNTS_PER_BLOCK {
				break Some(who);
			}
		};

		if !batch.is_empty() {
			Pallet::<T>::send_accounts(batch)?;
		}
		Ok(maybe_last_key)
	}

	/// Withdraw a single account from the relay chain and return it in portable format.
	///
	/// `Ok(None)` means the account is deliberately not migrated; `Err` means it should have
	/// migrated but could not be withdrawn cleanly (the caller rolls it back and skips it).
	pub fn withdraw_account(
		who: &T::AccountId,
		info: AccountInfoFor<T>,
	) -> Result<Option<PortableAccount<AccountId32, u128>>, Error<T>> {
		if !Self::can_migrate(who, &info) {
			return Ok(None);
		}
		let free = info.data.free;
		let reserved = info.data.reserved;

		// A reserve is supposed to be backed by a consumer reference; accounts where it is not
		// have broken refcounts (a known on-chain anomaly). Unreserving them still works, but
		// makes `frame_system` log an anonymous "underflow in reducing consumer" error — name
		// the account here so the anomaly is attributable.
		if !reserved.is_zero() && frame_system::Pallet::<T>::consumers(who) == 0 {
			log::warn!(
				target: LOG_TARGET,
				"Account {who:?} has reserved balance but no consumer reference"
			);
		}

		// Deposits on the relay chain are unnamed reserves (`can_migrate` rejects named holds);
		// release them so the full balance is burnable.
		let not_unreserved = <T as Config>::Currency::unreserve(who, reserved);
		if !not_unreserved.is_zero() {
			defensive!("Reserved balance was not fully released");
			return Err(Error::<T>::FailedToWithdrawAccount);
		}

		// Releasing the reserve drops its consumer reference; anything left means some pallet
		// still references this account and deleting its balance would corrupt that state.
		if frame_system::Pallet::<T>::consumers(who) != 0 {
			return Err(Error::<T>::AccountReferenced);
		}

		let total = free.saturating_add(reserved);
		let burned = <T as Config>::Currency::burn_from(
			who,
			total,
			Preservation::Expendable,
			Precision::Exact,
			Fortitude::Polite,
		)
		.map_err(|_| Error::<T>::FailedToWithdrawAccount)?;
		defensive_assert!(burned == total, "burned the account's whole balance");

		RcMigratedBalance::<T>::try_mutate(|tracker| {
			tracker.migrated =
				tracker.migrated.checked_add(burned).ok_or(Error::<T>::BalanceAccounting)?;
			tracker.kept = tracker.kept.checked_sub(burned).ok_or(Error::<T>::BalanceAccounting)?;
			Ok::<(), Error<T>>(())
		})?;

		let mut holds = BoundedVec::default();
		if !reserved.is_zero() {
			holds
				.try_push(PortableHold {
					reason: PortableHoldReason::UnnamedReserve,
					amount: reserved,
				})
				.map_err(|_| Error::<T>::FailedToWithdrawAccount)?;
		}

		Ok(Some(PortableAccount { who: who.clone(), free, holds }))
	}

	/// Whether the account migrates at all. The rejections here are deliberate policy, not
	/// failures.
	pub fn can_migrate(who: &T::AccountId, info: &AccountInfoFor<T>) -> bool {
		let bytes: &[u8] = who.as_ref();
		if UNTRANSLATED_PREFIXES.iter().any(|prefix| bytes.starts_with(prefix)) {
			log::info!(target: LOG_TARGET, "Keeping sovereign/module account {who:?} on the RC");
			return false;
		}

		let data = &info.data;
		let total = data.free.saturating_add(data.reserved);
		if total < <T as Config>::Currency::minimum_balance() {
			// Below-ED accounts only exist via external provider references (e.g. system
			// accounts); what happens to them is undecided.
			log::info!(target: LOG_TARGET, "Keeping below-ED account {who:?} on the RC");
			return false;
		}

		// Locks, freezes and named holds should no longer occur on the relay chain post-AHM
		// (staking and governance are gone); translating them is not implemented.
		if !data.frozen.is_zero() ||
			!pallet_balances::Locks::<T>::get(who).is_empty() ||
			!pallet_balances::Freezes::<T>::get(who).is_empty() ||
			!pallet_balances::Holds::<T>::get(who).is_empty()
		{
			log::warn!(
				target: LOG_TARGET,
				"Keeping account {who:?} with untranslatable locks/freezes/holds on the RC"
			);
			return false;
		}

		true
	}
}
