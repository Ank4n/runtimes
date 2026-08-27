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
//! - reserved balance, up to what the registrar and HRMP pallets record as this account's deposits,
//!   goes to the **Coretime chain** as a hold (re-attributed by the later stages);
//! - a small working buffer of free balance (`Config::CtFreeBuffer`) follows the deposit to
//!   Coretime;
//! - all remaining free balance is **teleported to Asset Hub**, where the owners' phase-1 funds
//!   already live.
//!
//! Exception: a delegator that never signed (`nonce == 0`) and grants an `Any` proxy is treated
//! as a keyless pure proxy — its delegate keeps full control only where the definitions are
//! recreated (the Coretime chain, by the proxy stage), so ALL of its balance goes there instead
//! of Asset Hub.
//!
//! Nothing stays behind: reserve that no pallet's deposit records account for (a known on-chain
//! anomaly) also travels to the Coretime chain, under its own hold reason, and stays parked there
//! for investigation.
//!
//! Para sovereign accounts are included: their child-sovereign id (`para…`) is translated to the
//! sibling id (`sibl…`) that represents the same para on a parachain.

use crate::*;
use frame_support::{
	defensive_assert,
	traits::tokens::{Fortitude, Precision, Preservation},
};
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
	/// - proxy deposits ([`ExpectedProxyReserve`]): per delegator with at least one portable
	///   definition — they travel under their own hold reason and are resized when the
	///   definitions arrive;
	/// - refunded ([`ExpectedRefundReserve`]): proxy deposits of delegators none of whose
	///   definitions travel — deposits whose purpose does not continue.
	///
	/// Called once by `AccountsInit`. Returns the number of records indexed.
	pub fn build_expected_reserves() -> u32 {
		let mut records = 0u32;
		let add_ct = |who: T::AccountId, amount: u128| {
			if !amount.is_zero() {
				ExpectedCtReserve::<T>::mutate(&who, |v| *v = v.saturating_add(amount));
			}
		};
		let add_proxy = |who: T::AccountId, amount: u128| {
			if !amount.is_zero() {
				ExpectedProxyReserve::<T>::mutate(&who, |v| *v = v.saturating_add(amount));
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
		for (who, (defs, deposit)) in pallet_proxy::Proxies::<T>::iter() {
			let travels = defs
				.iter()
				.any(|def| TryInto::<PortableProxyType>::try_into(def.proxy_type.clone()).is_ok());
			if travels {
				add_proxy(who, deposit);
			} else {
				add_refund(who, deposit);
			}
			records += 1;
		}
		// Pending open-channel requests migrate to the Coretime chain with their deposits, so
		// the sender sovereigns' request deposits are CT-bound like channel deposits.
		for (id, request) in runtime_parachains::hrmp::HrmpOpenChannelRequests::<T>::iter() {
			add_ct(id.sender.into_account_truncating(), request.sender_deposit);
			records += 1;
		}
		records
	}

	/// The account id under which `who`'s balances continue on the destination chains.
	///
	/// Child para sovereigns become sibling sovereigns (the same para as seen from a sibling
	/// parachain); everyone else keeps their address.
	pub fn translate_destination(who: &T::AccountId) -> AccountId32 {
		match ParaId::try_from_account(who) {
			Some(para_id) => migrator_types::sibling_account(para_id.into()),
			None => who.clone(),
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
		// Balance-tracker deltas of this block's successful withdrawals; applied in one write at
		// the end instead of one storage mutation per account.
		let (mut ct_hold_sum, mut ct_free_sum, mut ah_free_sum) = (0u128, 0u128, 0u128);
		let mut processed = 0u32;
		let maybe_last_key = loop {
			let Some((who, info)) = iter.next() else { break None };
			processed += 1;

			// Each account is withdrawn in its own transaction: a failure rolls back that
			// account only, so it is skipped whole, never half-withdrawn.
			match with_rollback(|| Self::withdraw_account(&who, info)) {
				Ok(Some(Withdrawal { ct, ah })) => {
					if let Some(account) = ct {
						ct_hold_sum = ct_hold_sum
							.saturating_add(account.holds.iter().map(|h| h.amount).sum());
						ct_free_sum = ct_free_sum.saturating_add(account.free);
						ct_batch.push(account);
					}
					if let Some((who, amount)) = ah {
						ah_free_sum = ah_free_sum.saturating_add(amount);
						ah_batch.push((who, amount));
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

		let burned = ct_hold_sum.saturating_add(ct_free_sum).saturating_add(ah_free_sum);
		if burned > 0 {
			RcMigratedBalance::<T>::try_mutate(|t| {
				t.kept = t.kept.checked_sub(burned).ok_or(Error::<T>::BalanceAccounting)?;
				t.ct_reserved =
					t.ct_reserved.checked_add(ct_hold_sum).ok_or(Error::<T>::BalanceAccounting)?;
				t.ct_free =
					t.ct_free.checked_add(ct_free_sum).ok_or(Error::<T>::BalanceAccounting)?;
				t.ah_free =
					t.ah_free.checked_add(ah_free_sum).ok_or(Error::<T>::BalanceAccounting)?;
				Ok::<(), Error<T>>(())
			})?;
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

		// What the owning pallets say this account deposited: registrar/HRMP deposits continue on
		// the Coretime chain, proxy deposits travel there under their own reason (or are refunded
		// when no definition travels). Anything beyond is money whose origin is unknown (a true
		// anomaly): it travels too — nothing stays behind — but under its own hold reason, parked
		// at the destination for investigation.
		let (expected_ct, expected_proxy, expected_refund) = if reserved.is_zero() {
			(0, 0, 0)
		} else {
			(
				ExpectedCtReserve::<T>::get(who),
				ExpectedProxyReserve::<T>::get(who),
				ExpectedRefundReserve::<T>::get(who),
			)
		};

		// A reserve is supposed to be backed by a consumer reference; accounts where it is not
		// have broken refcounts (a known on-chain anomaly). Unreserving them still works, but
		// makes `frame_system` log an anonymous "underflow in reducing consumer" error — name
		// the account here so the anomaly is attributable.
		if !reserved.is_zero() && info.consumers == 0 {
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

		// The split: CT-bound deposits, proxy deposits and unattributed reserve → CT holds (one
		// per reason); refunded deposits become liquid; working buffer → CT free; the rest → AH
		// free. Free balance below AH's ED cannot teleport into a fresh account, so such dust
		// follows the deposit to CT instead (only deposit holders can be in this situation:
		// everyone else has free >= the RC ED, which exceeds AH's).
		//
		// Exception: a never-signed delegator granting an `Any` proxy is (or must be treated as)
		// a keyless pure proxy. Its delegate keeps full control only on the Coretime chain, where
		// the proxy stage recreates the definitions, so ALL of its liquid balance goes there.
		let ct_hold = reserved.min(expected_ct);
		let proxy_hold = reserved.saturating_sub(ct_hold).min(expected_proxy);
		let refunded =
			reserved.saturating_sub(ct_hold).saturating_sub(proxy_hold).min(expected_refund);
		let unattributed =
			reserved.saturating_sub(ct_hold).saturating_sub(proxy_hold).saturating_sub(refunded);
		if !refunded.is_zero() {
			Pallet::<T>::deposit_event(Event::DepositRefunded {
				who: who.clone(),
				amount: refunded,
			});
		}
		if !unattributed.is_zero() {
			Pallet::<T>::deposit_event(Event::UnattributedReserve {
				who: who.clone(),
				amount: unattributed,
			});
		}
		let held = ct_hold.saturating_add(proxy_hold).saturating_add(unattributed);
		let liquid = free.saturating_add(refunded);
		let pure_like = info.nonce.is_zero() &&
			pallet_proxy::Proxies::<T>::get(who).0.iter().any(|def| {
				matches!(def.proxy_type.clone().try_into(), Ok(PortableProxyType::Any))
			});
		let mut ct_free = if pure_like {
			liquid
		} else if held.is_zero() {
			0
		} else {
			liquid.min(T::CtFreeBuffer::get())
		};
		let mut ah_free = liquid.saturating_sub(ct_free);
		if !ah_free.is_zero() && ah_free < T::AhExistentialDeposit::get() && !held.is_zero() {
			ct_free = ct_free.saturating_add(ah_free);
			ah_free = 0;
		}

		let dest = Self::translate_destination(who);
		let ct = if held.is_zero() && ct_free.is_zero() {
			None
		} else {
			let mut holds = BoundedVec::default();
			for (reason, amount) in [
				(PortableHoldReason::UnnamedReserve, ct_hold),
				(PortableHoldReason::ProxyDeposit, proxy_hold),
				(PortableHoldReason::UnattributedReserve, unattributed),
			] {
				if !amount.is_zero() {
					holds
						.try_push(PortableHold { reason, amount })
						.map_err(|_| Error::<T>::FailedToWithdrawAccount)?;
				}
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
