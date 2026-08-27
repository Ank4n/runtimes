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

//! Relay-chain side of the registrar + HRMP migration to the Coretime chain.
//!
//! Drives the migration stage machine: drains account balances and legacy `paras_registrar` and
//! `hrmp` state together with their deposits and sends everything to the counterpart
//! `pallet-ct-migrator` over XCM. Temporary pallet; removed once the migration is complete.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod accounts;
pub mod hrmp;
pub mod proxy;
pub mod registrar;

pub use pallet::*;

use alloc::{vec, vec::Vec};
use frame_support::{
	defensive,
	pallet_prelude::*,
	traits::{
		fungible::{Inspect, Mutate},
		ReservableCurrency,
	},
};
use frame_system::pallet_prelude::*;
use migrator_types::{
	with_rollback, PortableAccount, PortableHold, PortableHoldReason, PortableHrmpChannel,
	PortableHrmpRequest, PortableParaInfo, PortableProxy, PortableProxyType,
};
use polkadot_parachain_primitives::primitives::{HrmpChannelId, Id as ParaId};
use polkadot_runtime_common::paras_registrar;
use sp_runtime::AccountId32;
use xcm::prelude::*;

const LOG_TARGET: &str = "runtime::rc2-migrator";

pub type MigrationStageOf<T> =
	MigrationStage<<T as frame_system::Config>::AccountId, BlockNumberFor<T>>;

/// Maximum number of accounts packed into one XCM message.
///
/// An encoded [`PortableAccount`] is ~65 bytes, keeping the message far below the DMP size limit.
pub const MAX_ACCOUNTS_PER_XCM: u32 = 100;

/// Maximum number of accounts processed per relay-chain block.
///
/// Also bounds the unbenchmarked work of both this pallet's `on_initialize` and the resulting
/// `receive_accounts` calls on the Coretime chain.
pub const MAX_ACCOUNTS_PER_BLOCK: u32 = 300;

/// Batch and per-block limits for the registrar and HRMP stages. Their record counts are small
/// (dozens to hundreds on Polkadot), so one limit serves both.
pub const MAX_RECORDS_PER_XCM: u32 = 50;
pub const MAX_RECORDS_PER_BLOCK: u32 = 100;

/// Maximum beneficiaries in one teleport message to Asset Hub: one `DepositAsset` instruction
/// each, and an XCM message decodes at most 100 instructions.
pub const MAX_TELEPORTS_PER_XCM: u32 = 40;

/// How long the migration parks in `CoolOff` for manual verification before finishing.
pub const COOL_OFF_BLOCKS: u32 = 10;

/// Progress of the migration. Advanced by `on_initialize`.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, Default, PartialEq, Eq, Debug, TypeInfo)]
pub enum MigrationStage<AccountId, BlockNumber> {
	#[default]
	Pending,
	Scheduled {
		start: BlockNumber,
	},
	/// Halts the machine while keeping the migration "ongoing" (call filters stay engaged).
	/// Entered and left only via `force_set_stage`.
	Paused,
	/// Waiting for the Coretime chain to confirm that it is ready to receive data.
	WaitingForCt,
	AccountsInit,
	AccountsOngoing {
		last_key: Option<AccountId>,
	},
	AccountsDone,
	/// Migrates portable proxy definitions to the Coretime chain.
	ProxyInit,
	ProxyOngoing {
		last_key: Option<AccountId>,
	},
	ProxyDone,
	RegistrarInit,
	RegistrarOngoing {
		last_key: Option<ParaId>,
	},
	RegistrarDone,
	HrmpInit,
	HrmpOngoing {
		last_key: Option<HrmpChannelId>,
	},
	HrmpDone,
	/// Empty the configured leftover pots (old treasury, …) and reap below-ED dust; everything
	/// teleports to `Config::SweepBeneficiary` on Asset Hub.
	Sweep,
	/// Burn the audited amount of issuance that no account holds (see `Config::TiCorrection`).
	TiCorrection,
	/// All data sent; waiting for manual verification before finishing.
	CoolOff {
		end_at: BlockNumber,
	},
	MigrationDone,
}

impl<AccountId, BlockNumber> MigrationStage<AccountId, BlockNumber> {
	pub fn is_finished(&self) -> bool {
		matches!(self, Self::MigrationDone)
	}

	pub fn is_ongoing(&self) -> bool {
		!matches!(self, Self::Pending | Self::Scheduled { .. } | Self::MigrationDone)
	}
}

/// Payload of a `Transact` sent to the Coretime chain.
///
/// Manual call encoding: the enum indices must match `CtMigrator`'s pallet index in the Coretime
/// `construct_runtime` and the `#[pallet::call_index]` attributes in `pallet-ct-migrator`. The
/// integration test decodes every sent `Transact` with the real Coretime `RuntimeCall`, which
/// catches drift.
#[derive(Encode)]
pub enum CtRuntimeCall {
	#[codec(index = 100)]
	CtMigrator(CtMigratorCall),
}

#[derive(Encode)]
pub enum CtMigratorCall {
	#[codec(index = 0)]
	ReceiveAccounts { accounts: Vec<PortableAccount<AccountId32, u128>> },
	#[codec(index = 1)]
	ReceiveRegistrar {
		paras: Vec<PortableParaInfo<AccountId32, u128>>,
		next_free_para_id: Option<u32>,
	},
	#[codec(index = 2)]
	ReceiveHrmp { channels: Vec<PortableHrmpChannel<u128>> },
	#[codec(index = 3)]
	FinishMigration { rc_kept: u128, rc_migrated: u128 },
	#[codec(index = 4)]
	ReceiveProxies { proxies: Vec<PortableProxy<AccountId32>> },
	#[codec(index = 5)]
	ReceiveHrmpRequests { requests: Vec<PortableHrmpRequest<u128>> },
}

/// Balance conservation bookkeeping for the migration.
///
/// `kept + ct_reserved + ct_free + ah_free` must always equal the relay-chain total issuance
/// recorded when the accounts stage started; the invariant checks assert against this.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Clone,
	Default,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub struct MigratedBalances<Balance> {
	/// Balance that remains on the relay chain.
	pub kept: Balance,
	/// Deposits burned here and re-established as holds on the Coretime chain.
	pub ct_reserved: Balance,
	/// Free working buffer burned here and minted liquid on the Coretime chain.
	pub ct_free: Balance,
	/// Free balance burned here and teleported to Asset Hub.
	pub ah_free: Balance,
	/// Phantom issuance burned by the `TiCorrection` stage (issuance no account held).
	pub ti_corrected: Balance,
}

impl MigratedBalances<u128> {
	/// Everything that went to the Coretime chain; what `finish_migration` reconciles against.
	pub fn migrated_ct(&self) -> u128 {
		self.ct_reserved.saturating_add(self.ct_free)
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config]
	pub trait Config:
		frame_system::Config<
			AccountId = AccountId32,
			AccountData = pallet_balances::AccountData<u128>,
		> + pallet_balances::Config<Balance = u128>
		// The `Currency` equalities pin the deposit balance types to u128; the `ProxyType`
		// bound is where the runtime declares which proxy permissions travel to the Coretime
		// chain (untranslatable ones stay here).
		+ paras_registrar::Config<Currency = pallet_balances::Pallet<Self>>
		+ runtime_parachains::hrmp::Config
		+ pallet_proxy::Config<
			Currency = pallet_balances::Pallet<Self>,
			ProxyType: TryInto<PortableProxyType>,
		>
	{
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Native currency.
		type Currency: Mutate<Self::AccountId, Balance = u128>
			+ ReservableCurrency<Self::AccountId, Balance = u128>;

		/// Router for XCM messages to the Coretime chain.
		type SendXcm: SendXcm;

		/// Para id of the Coretime chain.
		type CtParaId: Get<u32>;

		/// Para id of Asset Hub, the destination of teleported free balances.
		type AhParaId: Get<u32>;

		/// Working buffer of free balance that follows a migrated deposit to the Coretime chain,
		/// so deposit owners can pay fees and future deposits there without a teleport first.
		type CtFreeBuffer: Get<u128>;

		/// Asset Hub's existential deposit. Free balance below this cannot be teleported into a
		/// fresh account; such dust follows the deposit to the Coretime chain instead.
		type AhExistentialDeposit: Get<u128>;

		/// Leftover module pots to empty in the `Sweep` stage (e.g. the old treasury pot).
		/// Their full balance teleports to `SweepBeneficiary`.
		type SweepAccounts: Get<Vec<AccountId32>>;

		/// Where swept pots and dust land on Asset Hub — the treasury / DAP buffer account
		/// designated by governance.
		type SweepBeneficiary: Get<AccountId32>;

		/// The audited amount of total issuance that no account holds ("phantom issuance"),
		/// burned by the `TiCorrection` stage at the end of the migration.
		///
		/// Governance-legible tunable: measured off-chain ahead of the migration
		/// (`balance_census` prints the exact planck value) and pinned here. The stage burns
		/// `min(this, measured-on-chain)` — anything unaccounted beyond it is left for
		/// investigation, and a measured value *below* it is reported as an anomaly; the stage
		/// never burns issuance that an account actually holds.
		type TiCorrection: Get<u128>;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::storage]
	#[pallet::unbounded]
	pub type RcMigrationStage<T: Config> = StorageValue<_, MigrationStageOf<T>, ValueQuery>;

	/// Balance kept on the relay chain versus migrated away. Set up by the accounts stage.
	#[pallet::storage]
	pub type RcMigratedBalance<T: Config> = StorageValue<_, MigratedBalances<u128>, ValueQuery>;

	/// How much reserved balance each account is expected to carry to the Coretime chain:
	/// the registrar deposits recorded for it as manager plus the HRMP channel deposits recorded
	/// for it as (child) para sovereign. Built by `AccountsInit` from the owning pallets' state —
	/// the recorded deposit fields are the routing source of truth, the anonymous reserves are
	/// only trusted up to this amount.
	#[pallet::storage]
	pub type ExpectedCtReserve<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, u128, ValueQuery>;

	/// How much reserved balance each account gets *refunded* (released and teleported to Asset
	/// Hub as free): proxy deposits, whose entries are recreated deposit-free or backed at the
	/// destination's own rates. Built by `AccountsInit` like [`ExpectedCtReserve`].
	#[pallet::storage]
	pub type ExpectedRefundReserve<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, u128, ValueQuery>;

	#[pallet::error]
	pub enum Error<T> {
		/// Sending an XCM message to the Coretime chain failed.
		XcmSendFailed,
		/// The account balance could not be fully withdrawn.
		FailedToWithdrawAccount,
		/// The account still has consumer references after releasing its reserves.
		AccountReferenced,
		/// The migrated/kept balance bookkeeping would overflow.
		BalanceAccounting,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		StageTransition {
			old: MigrationStageOf<T>,
			new: MigrationStageOf<T>,
		},
		/// A batch of withdrawn accounts was sent to the Coretime chain.
		AccountsBatchSent {
			count: u32,
		},
		/// A batch of free balances was teleported to Asset Hub.
		AccountsTeleported {
			count: u32,
			amount: u128,
		},
		/// An account was kept whole on the relay chain: it carries reserve that no pallet's
		/// deposit records account for. Money whose destination is unknown does not move.
		AccountHeldBack {
			who: AccountId32,
			free: u128,
			reserved: u128,
		},
		/// A proxy deposit was released and refunded: it travels to Asset Hub as free balance.
		DepositRefunded {
			who: AccountId32,
			amount: u128,
		},
		/// A batch of portable proxy sets was sent to the Coretime chain.
		ProxyBatchSent {
			count: u32,
		},
		/// Pending HRMP open-channel requests were sent to the Coretime chain.
		HrmpRequestsSent {
			count: u32,
		},
		/// A leftover pot was emptied; its balance teleports to the sweep beneficiary on AH.
		AccountSwept {
			who: AccountId32,
			amount: u128,
		},
		/// Below-ED dust accounts were reaped; the sum teleports to the sweep beneficiary.
		DustSwept {
			count: u32,
			amount: u128,
		},
		/// Phantom issuance burned: `burned = min(expected, unaccounted)`. Any
		/// `unaccounted - burned` remainder is left on the books for investigation.
		TiCorrected {
			expected: u128,
			unaccounted: u128,
			burned: u128,
		},
		/// The measured unaccounted issuance was BELOW the audited expectation — the phantom
		/// shrank since it was measured, which no known mechanism explains. Observability only;
		/// the correction still burned the measured amount.
		TiCorrectionAnomaly {
			expected: u128,
			unaccounted: u128,
		},
		/// A batch of drained registrar records was sent to the Coretime chain.
		RegistrarBatchSent {
			count: u32,
		},
		/// A batch of drained HRMP channel records was sent to the Coretime chain.
		HrmpBatchSent {
			count: u32,
		},
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(now: BlockNumberFor<T>) -> Weight {
			Self::progress_migration(now)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Set the migration stage directly.
		///
		/// Recovery and testing hook; the stage machine normally advances itself in
		/// `on_initialize`.
		#[pallet::call_index(0)]
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub fn force_set_stage(origin: OriginFor<T>, stage: MigrationStageOf<T>) -> DispatchResult {
			ensure_root(origin)?;

			Self::transition(stage);
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		fn progress_migration(now: BlockNumberFor<T>) -> Weight {
			match RcMigrationStage::<T>::get() {
				MigrationStage::Scheduled { start } if now >= start => {
					// The Coretime readiness handshake (`WaitingForCt`) is not implemented yet;
					// go straight to the first data stage.
					Self::transition(MigrationStage::AccountsInit);
					T::DbWeight::get().reads_writes(1, 1)
				},
				MigrationStage::AccountsInit => {
					let total_issuance = <T as Config>::Currency::total_issuance();
					RcMigratedBalance::<T>::put(MigratedBalances {
						kept: total_issuance,
						..Default::default()
					});
					let indexed = accounts::AccountsMigrator::<T>::build_expected_reserves();
					log::info!(
						target: LOG_TARGET,
						"Indexed expected reserves from {indexed} records"
					);
					Self::transition(MigrationStage::AccountsOngoing { last_key: None });
					T::DbWeight::get().reads_writes(2, 2)
				},
				MigrationStage::AccountsOngoing { last_key } => {
					// All of this block's withdrawals commit or roll back together, so a failed
					// XCM send cannot leave balances burned but never sent.
					Self::migrate_stage_step(
						|| accounts::AccountsMigrator::<T>::migrate_many(last_key),
						MigrationStage::AccountsDone,
						|last_key| MigrationStage::AccountsOngoing { last_key: Some(last_key) },
					);
					Self::placeholder_weight(MAX_ACCOUNTS_PER_BLOCK)
				},
				MigrationStage::AccountsDone => {
					Self::transition(MigrationStage::ProxyInit);
					T::DbWeight::get().reads_writes(1, 1)
				},
				MigrationStage::ProxyInit => {
					Self::transition(MigrationStage::ProxyOngoing { last_key: None });
					T::DbWeight::get().reads_writes(1, 1)
				},
				MigrationStage::ProxyOngoing { last_key } => {
					Self::migrate_stage_step(
						|| proxy::ProxyMigrator::<T>::migrate_many(last_key),
						MigrationStage::ProxyDone,
						|last_key| MigrationStage::ProxyOngoing { last_key: Some(last_key) },
					);
					Self::placeholder_weight(MAX_RECORDS_PER_BLOCK)
				},
				MigrationStage::ProxyDone => {
					Self::transition(MigrationStage::RegistrarInit);
					T::DbWeight::get().reads_writes(1, 1)
				},
				MigrationStage::RegistrarInit => {
					Self::migrate_stage_once(
						registrar::RegistrarMigrator::<T>::migrate_init,
						MigrationStage::RegistrarOngoing { last_key: None },
					);
					T::DbWeight::get().reads_writes(2, 2)
				},
				MigrationStage::RegistrarOngoing { last_key } => {
					Self::migrate_stage_step(
						|| registrar::RegistrarMigrator::<T>::migrate_many(last_key),
						MigrationStage::RegistrarDone,
						|last_key| MigrationStage::RegistrarOngoing { last_key: Some(last_key) },
					);
					Self::placeholder_weight(MAX_RECORDS_PER_BLOCK)
				},
				MigrationStage::RegistrarDone => {
					Self::transition(MigrationStage::HrmpInit);
					T::DbWeight::get().reads_writes(1, 1)
				},
				MigrationStage::HrmpInit => {
					Self::migrate_stage_once(
						hrmp::HrmpMigrator::<T>::drain_open_requests,
						MigrationStage::HrmpOngoing { last_key: None },
					);
					T::DbWeight::get().reads_writes(200, 200)
				},
				MigrationStage::HrmpOngoing { last_key } => {
					Self::migrate_stage_step(
						|| hrmp::HrmpMigrator::<T>::migrate_many(last_key),
						MigrationStage::HrmpDone,
						|last_key| MigrationStage::HrmpOngoing { last_key: Some(last_key) },
					);
					Self::placeholder_weight(MAX_RECORDS_PER_BLOCK)
				},
				MigrationStage::HrmpDone => {
					Self::transition(MigrationStage::Sweep);
					T::DbWeight::get().reads_writes(1, 1)
				},
				MigrationStage::Sweep => {
					Self::migrate_stage_once(Self::sweep_leftovers, MigrationStage::TiCorrection);
					// Placeholder: iterates every remaining account once for the dust pass.
					T::DbWeight::get().reads_writes(10_000, 500)
				},
				MigrationStage::TiCorrection => {
					Self::migrate_stage_once(
						Self::correct_total_issuance,
						MigrationStage::CoolOff { end_at: now + COOL_OFF_BLOCKS.into() },
					);
					// Placeholder: the unaccounted-issuance measurement iterates every remaining
					// account once.
					T::DbWeight::get().reads_writes(10_000, 10)
				},
				MigrationStage::CoolOff { end_at } if now >= end_at => {
					Self::transition(MigrationStage::MigrationDone);
					T::DbWeight::get().reads_writes(1, 1)
				},
				_ => T::DbWeight::get().reads(1),
			}
		}

		/// Run one block's worth of a cursor-driven data stage inside a storage transaction and
		/// advance the stage machine from the result. Same semantics for every stage: `Ok(None)`
		/// finishes the stage, `Ok(Some(key))` continues from the cursor next block, `Err` rolls
		/// the whole block back and retries the same key range.
		fn migrate_stage_step<K>(
			migrate: impl FnOnce() -> Result<Option<K>, Error<T>>,
			done: MigrationStageOf<T>,
			ongoing: impl FnOnce(K) -> MigrationStageOf<T>,
		) {
			match with_rollback(migrate) {
				Ok(None) => Self::transition(done),
				Ok(Some(last_key)) => Self::transition(ongoing(last_key)),
				Err(e) => {
					// Stage unchanged: the same key range is retried next block.
					defensive!("Data stage failed, retrying: {:?}", e);
				},
			}
		}

		/// Run a one-shot stage inside a storage transaction and advance to `next` on success.
		/// An `Err` rolls all of the stage's writes back and retries it whole next block.
		fn migrate_stage_once(
			work: impl FnOnce() -> Result<(), Error<T>>,
			next: MigrationStageOf<T>,
		) {
			match with_rollback(work) {
				Ok(()) => Self::transition(next),
				Err(e) => {
					defensive!("Stage failed, retrying: {:?}", e);
				},
			}
		}

		/// One block of a record-drain stage: pull `(key, record)` pairs from `iter`, convert and
		/// remove each via `drain`, flush batches of [`MAX_RECORDS_PER_XCM`] through `send`, and
		/// stop after [`MAX_RECORDS_PER_BLOCK`] records. Returns the cursor to continue from, or
		/// `None` once the map is exhausted.
		pub(crate) fn drain_records<K, V, P>(
			mut iter: impl Iterator<Item = (K, V)>,
			mut drain: impl FnMut(&K, V) -> P,
			send: impl Fn(Vec<P>) -> Result<(), Error<T>>,
		) -> Result<Option<K>, Error<T>> {
			let mut batch = Vec::new();
			let mut processed = 0u32;
			let maybe_last_key = loop {
				let Some((key, record)) = iter.next() else { break None };
				processed += 1;
				batch.push(drain(&key, record));

				if batch.len() >= MAX_RECORDS_PER_XCM as usize {
					send(core::mem::take(&mut batch))?;
				}
				if processed >= MAX_RECORDS_PER_BLOCK {
					break Some(key);
				}
			};

			if !batch.is_empty() {
				send(batch)?;
			}
			Ok(maybe_last_key)
		}

		/// Placeholder until the migrator gets benchmarks: a deliberate overestimate of one
		/// block's work on up to `n` items.
		fn placeholder_weight(n: u32) -> Weight {
			T::DbWeight::get().reads_writes((n * 4) as u64, (n * 4) as u64)
		}

		pub(crate) fn transition(new: MigrationStageOf<T>) {
			let old = RcMigrationStage::<T>::get();
			RcMigrationStage::<T>::put(new.clone());
			log::info!(target: LOG_TARGET, "Stage transition: {old:?} -> {new:?}");
			Self::deposit_event(Event::StageTransition { old, new });
		}

		/// Send a batch of withdrawn accounts to the Coretime chain.
		pub(crate) fn send_accounts(
			accounts: Vec<PortableAccount<AccountId32, u128>>,
		) -> Result<(), Error<T>> {
			let count = accounts.len() as u32;
			Self::send_to_ct(CtMigratorCall::ReceiveAccounts { accounts })?;
			Self::deposit_event(Event::AccountsBatchSent { count });
			Ok(())
		}

		/// Send a batch of drained registrar records to the Coretime chain.
		pub(crate) fn send_registrar(
			paras: Vec<PortableParaInfo<AccountId32, u128>>,
			next_free_para_id: Option<u32>,
		) -> Result<(), Error<T>> {
			let count = paras.len() as u32;
			Self::send_to_ct(CtMigratorCall::ReceiveRegistrar { paras, next_free_para_id })?;
			Self::deposit_event(Event::RegistrarBatchSent { count });
			Ok(())
		}

		/// Send a batch of portable proxy sets to the Coretime chain.
		pub(crate) fn send_proxies(
			proxies: Vec<PortableProxy<AccountId32>>,
		) -> Result<(), Error<T>> {
			let count = proxies.len() as u32;
			Self::send_to_ct(CtMigratorCall::ReceiveProxies { proxies })?;
			Self::deposit_event(Event::ProxyBatchSent { count });
			Ok(())
		}

		/// Send a batch of pending HRMP open-channel requests to the Coretime chain.
		pub(crate) fn send_hrmp_requests(
			requests: Vec<PortableHrmpRequest<u128>>,
		) -> Result<(), Error<T>> {
			Self::send_to_ct(CtMigratorCall::ReceiveHrmpRequests { requests })
		}

		/// Send a batch of drained HRMP channel records to the Coretime chain.
		pub(crate) fn send_hrmp(channels: Vec<PortableHrmpChannel<u128>>) -> Result<(), Error<T>> {
			let count = channels.len() as u32;
			Self::send_to_ct(CtMigratorCall::ReceiveHrmp { channels })?;
			Self::deposit_event(Event::HrmpBatchSent { count });
			Ok(())
		}

		/// Signal to the Coretime chain that all data has been sent.
		pub(crate) fn send_finish(rc_kept: u128, rc_migrated: u128) -> Result<(), Error<T>> {
			Self::send_to_ct(CtMigratorCall::FinishMigration { rc_kept, rc_migrated })
		}

		/// Empty the configured leftover pots and reap below-ED dust; teleport the proceeds to
		/// the sweep beneficiary on Asset Hub.
		fn sweep_leftovers() -> Result<(), Error<T>> {
			use frame_support::traits::tokens::{Fortitude, Precision, Preservation};
			let mut total: u128 = 0;

			// The configured pots (old treasury etc.): full free balance, no reserves expected.
			for who in T::SweepAccounts::get() {
				let amount = frame_system::Account::<T>::get(&who).data.free;
				if amount == 0 {
					continue;
				}
				let burned = <T as Config>::Currency::burn_from(
					&who,
					amount,
					Preservation::Expendable,
					Precision::Exact,
					Fortitude::Polite,
				)
				.map_err(|_| Error::<T>::FailedToWithdrawAccount)?;
				// The treasury pot was book-kept as inactive issuance; reactivate what leaves so
				// the issuance accounting stays consistent.
				pallet_balances::InactiveIssuance::<T>::mutate(|i| *i = i.saturating_sub(burned));
				total = total.checked_add(burned).ok_or(Error::<T>::BalanceAccounting)?;
				Self::deposit_event(Event::AccountSwept { who, amount: burned });
			}

			// Below-ED dust: reap accounts that nothing references and no hold complicates.
			let (mut dust_count, mut dust_amount) = (0u32, 0u128);
			let ed = <T as Config>::Currency::minimum_balance();
			for (who, info) in frame_system::Account::<T>::iter() {
				let d = &info.data;
				if d.free.saturating_add(d.reserved) >= ed ||
					d.free == 0 || d.reserved != 0 ||
					info.consumers != 0 ||
					!pallet_balances::Holds::<T>::get(&who).is_empty()
				{
					continue;
				}
				let burned = <T as Config>::Currency::burn_from(
					&who,
					d.free,
					Preservation::Expendable,
					Precision::Exact,
					Fortitude::Polite,
				)
				.map_err(|_| Error::<T>::FailedToWithdrawAccount)?;
				dust_count += 1;
				dust_amount =
					dust_amount.checked_add(burned).ok_or(Error::<T>::BalanceAccounting)?;
			}
			if dust_count > 0 {
				total = total.checked_add(dust_amount).ok_or(Error::<T>::BalanceAccounting)?;
				Self::deposit_event(Event::DustSwept { count: dust_count, amount: dust_amount });
			}

			if total == 0 {
				return Ok(());
			}
			RcMigratedBalance::<T>::try_mutate(|t| {
				t.kept = t.kept.checked_sub(total).ok_or(Error::<T>::BalanceAccounting)?;
				t.ah_free = t.ah_free.checked_add(total).ok_or(Error::<T>::BalanceAccounting)?;
				Ok::<(), Error<T>>(())
			})?;
			Self::send_teleport(vec![(T::SweepBeneficiary::get(), total)])
		}

		/// Burn the audited phantom issuance and send the finish signal.
		///
		/// Measures the issuance no account holds, burns `min(expected, measured)` — never
		/// touching issuance that an account actually backs — and reports via events: a
		/// remainder above the expectation stays on the books for investigation, a measurement
		/// below it is an explicit anomaly.
		fn correct_total_issuance() -> Result<(), Error<T>> {
			let expected = T::TiCorrection::get();
			let in_accounts: u128 = frame_system::Account::<T>::iter_values()
				.map(|a| a.data.free.saturating_add(a.data.reserved))
				.sum();
			let ti = pallet_balances::TotalIssuance::<T>::get();
			let unaccounted = ti.saturating_sub(in_accounts);
			let burned = expected.min(unaccounted);

			if unaccounted < expected {
				log::error!(
					target: LOG_TARGET,
					"TI correction anomaly: expected {expected} unaccounted, measured {unaccounted}"
				);
				Self::deposit_event(Event::TiCorrectionAnomaly { expected, unaccounted });
			}

			// No account holds this balance, so there is nothing to burn *from*: the correction
			// is a direct issuance write, mirrored in the migration tracker so the conservation
			// invariant stays exact.
			pallet_balances::TotalIssuance::<T>::put(ti.saturating_sub(burned));
			RcMigratedBalance::<T>::try_mutate(|t| {
				t.kept = t.kept.checked_sub(burned).ok_or(Error::<T>::BalanceAccounting)?;
				t.ti_corrected =
					t.ti_corrected.checked_add(burned).ok_or(Error::<T>::BalanceAccounting)?;
				Ok::<(), Error<T>>(())
			})?;
			Self::deposit_event(Event::TiCorrected { expected, unaccounted, burned });

			let tracker = RcMigratedBalance::<T>::get();
			Self::send_finish(tracker.kept, tracker.migrated_ct())
		}

		/// Teleport a batch of free balances to their owners on Asset Hub.
		///
		/// A real teleport, not a `Transact`: the balances were already burned during withdrawal
		/// (the relay chain does no teleport tracking, `NoTeleportTracking`), and on Asset Hub
		/// each `DepositAsset` moves the amount out of the checking account — the "DOT out on the
		/// relay chain" ledger — so AH issuance stays constant and the ledger drains in lockstep
		/// with the relay chain. No receiving pallet is needed on Asset Hub.
		pub(crate) fn send_teleport(
			beneficiaries: Vec<(AccountId32, u128)>,
		) -> Result<(), Error<T>> {
			let count = beneficiaries.len() as u32;
			let total: u128 = beneficiaries.iter().map(|(_, amount)| amount).sum();
			// From Asset Hub's perspective DOT is the parent's asset.
			let dot = |amount: u128| Asset {
				id: AssetId(Location::parent()),
				fun: Fungibility::Fungible(amount),
			};

			let mut message = vec![
				UnpaidExecution { weight_limit: WeightLimit::Unlimited, check_origin: None },
				ReceiveTeleportedAsset(dot(total).into()),
			];
			for (who, amount) in beneficiaries {
				message.push(DepositAsset {
					assets: AssetFilter::Definite(dot(amount).into()),
					beneficiary: Location::new(
						0,
						[Junction::AccountId32 { network: None, id: who.into() }],
					),
				});
			}

			let dest = Location::new(0, [Parachain(T::AhParaId::get())]);
			send_xcm::<T::SendXcm>(dest, Xcm(message)).map_err(|e| {
				log::error!(target: LOG_TARGET, "Teleport to AH failed: {e:?}");
				Error::<T>::XcmSendFailed
			})?;

			Self::deposit_event(Event::AccountsTeleported { count, amount: total });
			Ok(())
		}

		/// Send a `pallet-ct-migrator` call to the Coretime chain.
		fn send_to_ct(call: CtMigratorCall) -> Result<(), Error<T>> {
			let call = CtRuntimeCall::CtMigrator(call);
			// `Superuser` converts to Root on the Coretime chain, which system chains grant the
			// relay-chain location; the `receive_*` calls check for Root.
			let message = Xcm(vec![
				UnpaidExecution { weight_limit: WeightLimit::Unlimited, check_origin: None },
				Transact {
					origin_kind: OriginKind::Superuser,
					fallback_max_weight: None,
					call: call.encode().into(),
				},
			]);

			let dest = Location::new(0, [Parachain(T::CtParaId::get())]);
			send_xcm::<T::SendXcm>(dest, message).map_err(|e| {
				log::error!(target: LOG_TARGET, "Sending to CT failed: {e:?}");
				Error::<T>::XcmSendFailed
			})?;
			Ok(())
		}
	}
}
