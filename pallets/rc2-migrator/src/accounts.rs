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

//! Accounts stage: withdraws account balances on the relay chain and routes the pieces to their
//! destinations.
//!
//! Per account, the balance splits by destination:
//! - reserved balance, up to what the registrar and HRMP pallets record as this account's
//!   deposits, goes to the **Coretime chain** as a hold (re-attributed by the later stages);
//! - a small working buffer of free balance (`CT_FREE_BUFFER`) follows the deposit to Coretime;
//! - all remaining free balance is **teleported to Asset Hub**, where the owners' phase-1 funds
//!   already live.
//!
//! Accounts whose reserve exceeds their recorded registrar/HRMP deposits (proxy deposits, known
//! anomalies) are kept whole on the relay chain for a later stage — money whose destination is
//! not designed yet does not move.
//!
//! Para sovereign accounts are included: their child-sovereign id (`para…`) is translated to the
//! sibling id (`sibl…`) that represents the same para on a parachain.

use crate::*;
use frame_support::{
	defensive_assert,
	traits::tokens::{Fortitude, Precision, Preservation},
};
use polkadot_parachain_primitives::primitives::Sibling;
use sp_runtime::traits::{AccountIdConversion, Zero};

pub type AccountInfoFor<T> = frame_system::AccountInfo<
	<T as frame_system::Config>::Nonce,
	pallet_balances::AccountData<u128>,
>;

/// Account-id prefixes that are never migrated: sibling-format sovereigns (an anomaly on a relay
/// chain) and pallet (module) accounts. Child sovereigns (`para`) ARE migrated, translated to
/// their sibling id.
const UNMIGRATED_PREFIXES: [&[u8]; 2] = [b"sibl", b"modl"];

/// Where the pieces of one withdrawn account go.
pub struct Withdrawal {
	/// Deposit hold plus working buffer, minted on the Coretime chain.
	pub ct: Option<PortableAccount<AccountId32, u128>>,
	/// Free balance teleported to Asset Hub: (beneficiary, amount).
	pub ah: Option<(AccountId32, u128)>,
}

pub struct AccountsMigrator<T>(PhantomData<T>);

impl<T: Config> AccountsMigrator<T> {
	/// Index every account's expected reserves from the owning pallets' records:
	/// - Coretime-bound ([`ExpectedCtReserve`]): registrar deposits per manager, HRMP channel
	///   deposits per (child) para sovereign;
	/// - refunded ([`ExpectedRefundReserve`]): proxy deposits per delegator and pending HRMP
	///   open-channel-request deposits per sovereign — deposits whose purpose does not continue.
	///
	/// Called once by `AccountsInit`. Returns the number of records indexed.
	pub fn build_expected_reserves() -> u32 {
		let mut records = 0u32;
		let add_ct = |who: T::AccountId, amount: u128| {
			if !amount.is_zero() {
				ExpectedCtReserve::<T>::mutate(&who, |v| *v = v.saturating_add(amount));
			}
		};
		let add_refund = |who: T::AccountId, amount: u128| {
			if !amount.is_zero() {
				ExpectedRefundReserve::<T>::mutate(&who, |v| *v = v.saturating_add(amount));
			}
		};

		for (_, info) in paras_registrar::Paras::<T>::iter() {
			add_ct(info.manager, info.deposit);
			records += 1;
		}
		for (id, channel) in runtime_parachains::hrmp::HrmpChannels::<T>::iter() {
			add_ct(id.sender.into_account_truncating(), channel.sender_deposit);
			add_ct(id.recipient.into_account_truncating(), channel.recipient_deposit);
			records += 1;
		}
		for (who, (_, deposit)) in pallet_proxy::Proxies::<T>::iter() {
			add_refund(who, deposit);
			records += 1;
		}
		for (id, request) in runtime_parachains::hrmp::HrmpOpenChannelRequests::<T>::iter() {
			add_refund(id.sender.into_account_truncating(), request.sender_deposit);
			records += 1;
		}
		records
	}

	/// The account id under which `who`'s balances continue on the destination chains.
	///
	/// Child para sovereigns become sibling sovereigns (the same para as seen from a sibling
	/// parachain); everyone else keeps their address.
	pub fn translate_destination(who: &T::AccountId) -> AccountId32 {
		let bytes: &[u8] = who.as_ref();
		if bytes.starts_with(b"para") && bytes[8..].iter().all(|b| *b == 0) {
			let para_id = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes; qed"));
			Sibling::from(ParaId::from(para_id)).into_account_truncating()
		} else {
			who.clone()
		}
	}

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

		let mut ct_batch = Vec::new();
		let mut ah_batch = Vec::new();
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
				Ok(Some(Withdrawal { ct, ah })) => {
					if let Some(account) = ct {
						ct_batch.push(account);
					}
					if let Some(beneficiary) = ah {
						ah_batch.push(beneficiary);
					}
				},
				Ok(None) => (),
				Err(e) => {
					log::warn!(target: LOG_TARGET, "Skipping account {who:?}: {e:?}");
				},
			}

			if ct_batch.len() >= MAX_ACCOUNTS_PER_XCM as usize {
				Pallet::<T>::send_accounts(core::mem::take(&mut ct_batch))?;
			}
			if ah_batch.len() >= MAX_TELEPORTS_PER_XCM as usize {
				Pallet::<T>::send_teleport(core::mem::take(&mut ah_batch))?;
			}
			if processed >= MAX_ACCOUNTS_PER_BLOCK {
				break Some(who);
			}
		};

		if !ct_batch.is_empty() {
			Pallet::<T>::send_accounts(ct_batch)?;
		}
		if !ah_batch.is_empty() {
			Pallet::<T>::send_teleport(ah_batch)?;
		}
		Ok(maybe_last_key)
	}

	/// Withdraw a single account from the relay chain and split it by destination.
	///
	/// `Ok(None)` means the account is deliberately not migrated; `Err` means it should have
	/// migrated but could not be withdrawn cleanly (the caller rolls it back and skips it).
	pub fn withdraw_account(
		who: &T::AccountId,
		info: AccountInfoFor<T>,
	) -> Result<Option<Withdrawal>, Error<T>> {
		if !Self::can_migrate(who, &info) {
			return Ok(None);
		}
		let free = info.data.free;
		let reserved = info.data.reserved;

		// Pinned by governance after the off-chain pre-flight (e.g. a possible pure proxy whose
		// control at the destination could not be verified): nothing of it moves.
		if HeldBackAccounts::<T>::contains_key(who) {
			log::info!(target: LOG_TARGET, "Holding back pinned account {who:?}");
			Pallet::<T>::deposit_event(Event::AccountHeldBack {
				who: who.clone(),
				free,
				reserved,
			});
			return Ok(None);
		}

		// Reserved balance is only trusted up to what the owning pallets say this account
		// deposited: registrar/HRMP deposits continue on the Coretime chain, proxy and pending
		// HRMP-request deposits are refunded. More reserve than the two together is money whose
		// origin is unknown (a true anomaly): the whole account stays on the RC.
		let expected_ct = ExpectedCtReserve::<T>::get(who);
		let expected_refund = ExpectedRefundReserve::<T>::get(who);
		if reserved > expected_ct.saturating_add(expected_refund) {
			log::info!(
				target: LOG_TARGET,
				"Holding back account {who:?}: reserved {reserved} > attributable \
				 {expected_ct} + {expected_refund}"
			);
			Pallet::<T>::deposit_event(Event::AccountHeldBack {
				who: who.clone(),
				free,
				reserved,
			});
			return Ok(None);
		}

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

		// The split: CT-bound deposits → CT hold; refunded deposits become liquid; working
		// buffer → CT free; the rest → AH free. Free balance below AH's ED cannot teleport into
		// a fresh account, so such dust follows the deposit to CT instead (only deposit holders
		// can be in this situation: everyone else has free >= the RC ED, which exceeds AH's).
		let ct_hold = reserved.min(expected_ct);
		let refunded = reserved.saturating_sub(ct_hold);
		if !refunded.is_zero() {
			Pallet::<T>::deposit_event(Event::DepositRefunded {
				who: who.clone(),
				amount: refunded,
			});
		}
		let liquid = free.saturating_add(refunded);
		let mut ct_free = if ct_hold.is_zero() { 0 } else { liquid.min(CT_FREE_BUFFER) };
		let mut ah_free = liquid.saturating_sub(ct_free);
		if !ah_free.is_zero() && ah_free < AH_EXISTENTIAL_DEPOSIT && !ct_hold.is_zero() {
			ct_free = ct_free.saturating_add(ah_free);
			ah_free = 0;
		}

		RcMigratedBalance::<T>::try_mutate(|t| {
			t.kept = t.kept.checked_sub(burned).ok_or(Error::<T>::BalanceAccounting)?;
			t.ct_reserved =
				t.ct_reserved.checked_add(ct_hold).ok_or(Error::<T>::BalanceAccounting)?;
			t.ct_free = t.ct_free.checked_add(ct_free).ok_or(Error::<T>::BalanceAccounting)?;
			t.ah_free = t.ah_free.checked_add(ah_free).ok_or(Error::<T>::BalanceAccounting)?;
			Ok::<(), Error<T>>(())
		})?;

		let dest = Self::translate_destination(who);
		let ct = if ct_hold.is_zero() && ct_free.is_zero() {
			None
		} else {
			let mut holds = BoundedVec::default();
			if !ct_hold.is_zero() {
				holds
					.try_push(PortableHold {
						reason: PortableHoldReason::UnnamedReserve,
						amount: ct_hold,
					})
					.map_err(|_| Error::<T>::FailedToWithdrawAccount)?;
			}
			Some(PortableAccount { who: dest.clone(), free: ct_free, holds })
		};
		let ah = (!ah_free.is_zero()).then(|| (dest, ah_free));

		Ok(Some(Withdrawal { ct, ah }))
	}

	/// Whether the account migrates at all. The rejections here are deliberate policy, not
	/// failures.
	pub fn can_migrate(who: &T::AccountId, info: &AccountInfoFor<T>) -> bool {
		let bytes: &[u8] = who.as_ref();
		if UNMIGRATED_PREFIXES.iter().any(|prefix| bytes.starts_with(prefix)) {
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
