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

#![doc = include_str!("accounts.md")]

#[cfg(test)]
mod tests;

use crate::{
	Config, Error, Event, ExpectedReserves, MigratedBalances, Pallet, RcMigratedBalance, LOG_TARGET,
};
use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::marker::PhantomData;
use frame_support::{
	defensive, defensive_assert,
	storage::with_storage_layer,
	traits::{
		fungible::{Inspect, Mutate},
		tokens::{Fortitude, Precision, Preservation},
		DefensiveTruncateFrom, Get, ReservableCurrency, StorePreimage,
	},
	BoundedVec, PalletId,
};
use migrator_types::{PortableAccount, PortableHold, PortableHoldReason, PortableProxyType};
use polkadot_runtime_common::paras_registrar;
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{AccountIdConversion, Zero},
	AccountId32, DispatchError, TypeId,
};

/// Maximum number of accounts processed per relay-chain block.
///
/// Bounds the work of one `on_initialize` here and of the resulting `receive_accounts` calls on
/// the Coretime chain.
pub const MAX_ACCOUNTS_PER_BLOCK: u32 = 300;

type NativeCurrency<T> = pallet_balances::Pallet<T>;
type AccountInfoFor<T> = frame_system::AccountInfo<
	<T as frame_system::Config>::Nonce,
	pallet_balances::AccountData<u128>,
>;

/// The expected composition of one account's reserved balance. See `ExpectedReserves`.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Clone,
	Copy,
	Default,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub struct ExpectedReserve {
	/// Continues on the Coretime chain as a `RegistrarDeposit` hold: registration deposits
	/// recorded for the account as manager.
	pub registrar: u128,
	/// Continues on the Coretime chain as an `HrmpDeposit` hold: channel and open-request
	/// deposits recorded for the account as (child) para sovereign.
	pub hrmp: u128,
	/// Continues on the Coretime chain as a `ProxyDeposit` hold (resized when the definitions
	/// arrive): proxy deposits of delegators with at least one portable definition.
	pub proxy: u128,
	/// Released and teleported to Asset Hub as free balance: deposits whose purpose ends with
	/// this chain (untranslatable proxy sets, multisig operations, announcements).
	pub refund: u128,
}

/// Where the pieces of one withdrawn account go.
#[derive(Debug, PartialEq, Eq)]
pub struct Withdrawal {
	/// Deposit hold plus working buffer, minted on the Coretime chain.
	pub ct: Option<PortableAccount<AccountId32, u128>>,
	/// Free balance teleported to Asset Hub: (beneficiary, amount).
	pub ah: Option<(AccountId32, u128)>,
}

/// Everything one block of withdrawals burned, ready to be shipped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BlockWithdrawals {
	/// Accounts to mint on the Coretime chain, holds included.
	pub ct: Vec<PortableAccount<AccountId32, u128>>,
	/// Free balance to teleport to Asset Hub: (beneficiary, amount).
	pub ah: Vec<(AccountId32, u128)>,
	/// Where the next block continues from; `None` once the account space is exhausted.
	pub last_key: Option<AccountId32>,
}

pub struct AccountsMigrator<T>(PhantomData<T>);

impl<T: Config> AccountsMigrator<T> {
	/// Preparation before the first block of withdrawals: releases every preimage deposit, seeds
	/// the conservation ledger with the current total issuance and indexes the expected reserves.
	/// Returns the number of deposit records indexed.
	///
	/// Safe to run again after the stage is rewound: the index is rebuilt and a ledger that is
	/// already seeded is left alone.
	pub fn init() -> u32 {
		// Before anything is measured: drop every preimage deposit, so accounts that hold one
		// are not skipped by `can_migrate`.
		Self::release_preimage_deposits();
		if !RcMigratedBalance::<T>::exists() {
			RcMigratedBalance::<T>::put(MigratedBalances {
				kept: NativeCurrency::<T>::total_issuance(),
				..Default::default()
			});
		}
		let indexed = Self::build_expected_reserves();
		log::info!(target: LOG_TARGET, "Indexed expected reserves from {indexed} records");
		indexed
	}

	/// Index every account's expected reserves from the owning pallets' records; see
	/// [`ExpectedReserve`]. The index is rebuilt from scratch. Returns the number of records
	/// indexed.
	fn build_expected_reserves() -> u32 {
		let _ = ExpectedReserves::<T>::clear(u32::MAX, None);
		let mut records = 0u32;
		// `slot` picks which expectation the amount accrues to.
		let add = |who: T::AccountId, amount: u128, slot: fn(&mut ExpectedReserve) -> &mut u128| {
			if !amount.is_zero() {
				ExpectedReserves::<T>::mutate(&who, |e| {
					let v = slot(e);
					*v = v.saturating_add(amount);
				});
			}
		};

		for (_, info) in paras_registrar::Paras::<T>::iter() {
			add(info.manager, info.deposit, |e| &mut e.registrar);
			records += 1;
		}
		for (id, channel) in runtime_parachains::hrmp::HrmpChannels::<T>::iter() {
			add(id.sender.into_account_truncating(), channel.sender_deposit, |e| &mut e.hrmp);
			add(id.recipient.into_account_truncating(), channel.recipient_deposit, |e| &mut e.hrmp);
			records += 1;
		}
		// Pending open-channel requests migrate to the Coretime chain with their deposits, so
		// the sender sovereigns' request deposits are Coretime-bound like channel deposits. A
		// confirmed request's recipient deposit is reserved but recorded nowhere until the
		// session boundary turns the request into a channel; in that window it is unattributed.
		// TODO(ahm-v2): close that window. From the migration start the lockdown refuses XCM from
		// every chain but the Coretime chain, so no request is confirmed after it; the warm-up
		// must also span a session boundary, so the ones confirmed before it are channels by the
		// time this runs.
		for (id, request) in runtime_parachains::hrmp::HrmpOpenChannelRequests::<T>::iter() {
			add(id.sender.into_account_truncating(), request.sender_deposit, |e| &mut e.hrmp);
			records += 1;
		}
		for (who, (defs, deposit)) in pallet_proxy::Proxies::<T>::iter() {
			let travels = defs
				.iter()
				.any(|def| TryInto::<PortableProxyType>::try_into(def.proxy_type.clone()).is_ok());
			if travels {
				add(who, deposit, |e| &mut e.proxy);
			} else {
				add(who, deposit, |e| &mut e.refund);
			}
			records += 1;
		}
		// Proxy announcement deposits, reserved on the announcer (the delegate). Announcements
		// are not migrated, so the deposit is refunded.
		for (announcer, (_, deposit)) in pallet_proxy::Announcements::<T>::iter() {
			add(announcer, deposit, |e| &mut e.refund);
			records += 1;
		}
		// Multisig operations are not migrated, so the deposit is refunded to the depositor.
		for (_, _, op) in pallet_multisig::Multisigs::<T>::iter() {
			add(op.depositor, op.deposit, |e| &mut e.refund);
			records += 1;
		}
		records
	}

	/// Withdraw accounts until the per-block limit is reached.
	///
	/// `manager` is the one account that stays funded here until the migration ends: it pays for
	/// the calls that drive the migration.
	///
	/// Each account is withdrawn in a storage transaction of its own, so one that cannot be
	/// withdrawn cleanly is skipped whole, never half-withdrawn.
	pub fn migrate_many(
		last_key: Option<T::AccountId>,
		manager: Option<&T::AccountId>,
	) -> BlockWithdrawals {
		let mut iter = match &last_key {
			Some(last_key) => frame_system::Account::<T>::iter_from_key(last_key),
			None => frame_system::Account::<T>::iter(),
		};

		let mut out = BlockWithdrawals::default();
		let mut processed = 0u32;
		out.last_key = loop {
			let Some((who, info)) = iter.next() else { break None };
			processed += 1;

			match with_storage_layer(|| Self::withdraw_account(&who, info, manager)) {
				Ok(Some(Withdrawal { ct, ah })) => {
					out.ct.extend(ct);
					out.ah.extend(ah);
				},
				Ok(None) => (),
				Err(e) => {
					defensive!("Error while migrating account");
					log::error!(target: LOG_TARGET, "Skipping account {who:?}: {e:?}");
					Pallet::<T>::deposit_event(Event::AccountSkipped { who: who.clone() });
				},
			}

			if processed >= MAX_ACCOUNTS_PER_BLOCK {
				break Some(who);
			}
		};

		// Ledger deltas of this block, applied in one write.
		let ct_hold_sum: u128 = out.ct.iter().flat_map(|a| &a.holds).map(|h| h.amount).sum();
		let ct_free_sum: u128 = out.ct.iter().map(|a| a.free).sum();
		let ah_free_sum: u128 = out.ah.iter().map(|(_, amount)| amount).sum();
		let burned = ct_hold_sum.saturating_add(ct_free_sum).saturating_add(ah_free_sum);
		if burned > 0 {
			RcMigratedBalance::<T>::mutate(|t| {
				defensive_assert!(t.kept >= burned, "burned more than the ledger has as kept");
				t.kept = t.kept.saturating_sub(burned);
				t.ct_reserved = t.ct_reserved.saturating_add(ct_hold_sum);
				t.ct_free = t.ct_free.saturating_add(ct_free_sum);
				t.ah_free = t.ah_free.saturating_add(ah_free_sum);
			});
		}
		out
	}

	/// Withdraw a single account from the relay chain and split it by destination.
	///
	/// `Ok(None)` means the account is deliberately not migrated; `Err` means it should have
	/// migrated but could not be withdrawn cleanly (the caller rolls it back and skips it).
	fn withdraw_account(
		who: &T::AccountId,
		info: AccountInfoFor<T>,
		manager: Option<&T::AccountId>,
	) -> Result<Option<Withdrawal>, DispatchError> {
		if !Self::can_migrate(who, &info, manager) {
			return Ok(None);
		}
		let free = info.data.free;
		let reserved = info.data.reserved;

		let expected = if reserved.is_zero() {
			ExpectedReserve::default()
		} else {
			ExpectedReserves::<T>::get(who)
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
		let not_unreserved = NativeCurrency::<T>::unreserve(who, reserved);
		if !not_unreserved.is_zero() {
			defensive!("Reserved balance was not fully released");
			return Err(Error::<T>::FailedToWithdrawAccount.into());
		}

		// Releasing the reserve drops its consumer reference; anything left means some pallet
		// still references this account (session keys being the known case) and the account
		// record must survive. Its balance must not: drain it to a zero-balance shell. The
		// fungible API keeps the ED for an account whose provider cannot be dropped, so the
		// account data is written directly, with total issuance adjusted to match.
		let total = free.saturating_add(reserved);
		if frame_system::Pallet::<T>::consumers(who) != 0 {
			frame_system::Account::<T>::mutate(who, |a| {
				a.data.free = 0;
				a.data.reserved = 0;
			});
			pallet_balances::TotalIssuance::<T>::mutate(|ti| *ti = ti.saturating_sub(total));
			Pallet::<T>::deposit_event(Event::AccountShellDrained {
				who: who.clone(),
				amount: total,
			});
		} else {
			NativeCurrency::<T>::burn_from(
				who,
				total,
				Preservation::Expendable,
				Precision::Exact,
				Fortitude::Polite,
			)?;
		}

		// The split, in priority order: registrar deposits, HRMP deposits, proxy deposits, then
		// refunds; whatever the expectations do not cover is unattributed. Each line consumes
		// from one remainder, so a live reserve that under-covers the records is attributed to
		// the deposits that continue first.
		let mut remainder = reserved;
		let mut consume = |cap: u128| {
			let taken = remainder.min(cap);
			remainder -= taken;
			taken
		};
		let registrar_hold = consume(expected.registrar);
		let hrmp_hold = consume(expected.hrmp);
		let proxy_hold = consume(expected.proxy);
		let refunded = consume(expected.refund);
		let unattributed = remainder;
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

		// Holds and unattributed reserve → Coretime holds (one per reason); refunded deposits
		// become liquid; working buffer → Coretime free; the rest → Asset Hub free, unless the
		// account is pure-like. Free balance below Asset Hub's ED cannot teleport into a fresh
		// account, so such dust follows the deposit to Coretime instead (only deposit holders can
		// be in this situation: everyone else has free >= the relay ED, which exceeds Asset Hub's).
		let held = registrar_hold
			.saturating_add(hrmp_hold)
			.saturating_add(proxy_hold)
			.saturating_add(unattributed);
		let liquid = free.saturating_add(refunded);
		let mut ct_free = if Self::is_pure_like(who, &info) {
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

		let dest = migrator_types::translate_destination(who);
		let ct = if held.is_zero() && ct_free.is_zero() {
			None
		} else {
			let holds = [
				(PortableHoldReason::RegistrarDeposit, registrar_hold),
				(PortableHoldReason::HrmpDeposit, hrmp_hold),
				(PortableHoldReason::ProxyDeposit, proxy_hold),
				(PortableHoldReason::UnattributedReserve, unattributed),
			]
			.into_iter()
			.filter(|(_, amount)| !amount.is_zero())
			.map(|(reason, amount)| PortableHold { reason, amount })
			.collect::<Vec<_>>();
			Some(PortableAccount {
				who: dest.clone(),
				free: ct_free,
				holds: BoundedVec::defensive_truncate_from(holds),
			})
		};
		let ah = (!ah_free.is_zero()).then_some((dest, ah_free));

		Ok(Some(Withdrawal { ct, ah }))
	}

	/// Release every preimage deposit on this chain.
	///
	/// [`Self::can_migrate`] refuses any account holding a *named* hold, so a preimage deposit
	/// would strand that account's entire balance. The blobs stay: the relay chain keeps hosting
	/// preimages.
	///
	/// - `Unrequested`: deleted, deposit returned.
	/// - `Requested` with a ticket: the blob is kept and only the ticket dropped, so a referendum
	///   that depends on it still resolves.
	/// - `Requested` without a ticket: already deposit-free. Skipped, since unnoting it would
	///   unrequest it (see `do_unnote_preimage`) and can delete a blob live governance waits on.
	fn release_preimage_deposits() {
		let with_deposit: Vec<_> = pallet_preimage::RequestStatusFor::<T>::iter()
			.filter(|(_, status)| match status {
				pallet_preimage::RequestStatus::Unrequested { .. } => true,
				pallet_preimage::RequestStatus::Requested { maybe_ticket, .. } =>
					maybe_ticket.is_some(),
			})
			.map(|(hash, _)| hash)
			.collect();

		let count = with_deposit.len();
		for hash in with_deposit {
			// The trait's manager path: no origin, no ownership check.
			<pallet_preimage::Pallet<T> as StorePreimage>::unnote(&hash);
		}
		if count > 0 {
			log::info!(target: LOG_TARGET, "Released {count} preimage deposit(s)");
		}
	}

	/// A delegator that never signed and grants a portable `Any` proxy: a pure proxy in all but
	/// name, since a pure is always created with `Any` and one that never signed cannot have been
	/// anything else. Its funds are reachable only through its delegate, so they follow the
	/// delegate to the Coretime chain whole. The deposit plays no part: a definition can carry
	/// none.
	fn is_pure_like(who: &T::AccountId, info: &AccountInfoFor<T>) -> bool {
		info.nonce.is_zero() &&
			pallet_proxy::Proxies::<T>::get(who).0.iter().any(|def| {
				matches!(def.proxy_type.clone().try_into(), Ok(PortableProxyType::Any))
			})
	}

	/// Whether `who` is an account class the migration never touches: pallet (module) accounts,
	/// whose pots the `Sweep` stage empties.
	pub(crate) fn is_unmigrated(who: &T::AccountId) -> bool {
		let bytes: &[u8] = who.as_ref();
		bytes.starts_with(&<PalletId as TypeId>::TYPE_ID)
	}

	/// Whether the account migrates at all. The rejections here are deliberate policy, not
	/// failures.
	fn can_migrate(
		who: &T::AccountId,
		info: &AccountInfoFor<T>,
		manager: Option<&T::AccountId>,
	) -> bool {
		// The manager pays for the calls that drive the migration, so it is the one account that
		// stays funded here until the migration ends.
		if manager.is_some_and(|manager| manager == who) {
			log::info!(target: LOG_TARGET, "Keeping the manager account {who:?} on the RC");
			return false;
		}
		if Self::is_unmigrated(who) {
			log::info!(target: LOG_TARGET, "Keeping module account {who:?} on the RC");
			return false;
		}

		let data = &info.data;
		let total = data.free.saturating_add(data.reserved);
		if total < NativeCurrency::<T>::minimum_balance() {
			// Below-ED accounts are left here for the dust sweep. This includes pure-like
			// delegators: their definitions still reach the Coretime chain, and a sub-ED
			// remainder is not worth a migration path of its own.
			log::debug!(target: LOG_TARGET, "Keeping below-ED account {who:?} on the RC");
			return false;
		}

		// Locks, freezes and named holds should no longer occur on the relay chain post-AHM
		// (staking and governance are gone); translating them is not implemented. `frozen` is
		// the largest lock or freeze, and holds are counted in `reserved`, so plain accounts need
		// no extra reads.
		if !data.frozen.is_zero() ||
			(!data.reserved.is_zero() && !pallet_balances::Holds::<T>::get(who).is_empty())
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
