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

//! Coretime-chain side of the registrar + HRMP migration.
//!
//! Ingests state sent by `pallet-rc2-migrator`, writing through the same code path as fresh
//! registrations so that migrated and newly created state are identical. Temporary pallet;
//! removed once the migration is complete.
//!
//! This crate also defines the portable payload types exchanged between the two migrators;
//! `pallet-rc2-migrator` depends on this crate for them, so the dependency only points from the
//! relay side to the coretime side.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use pallet::*;

use alloc::vec::Vec;
use frame_support::{
	defensive_assert,
	pallet_prelude::*,
	storage::{transactional::with_transaction_opaque_err, TransactionOutcome},
	traits::{
		fungible::{Inspect, InspectHold, Mutate, MutateHold},
		tokens::Precision,
	},
};
use frame_system::pallet_prelude::*;
use polkadot_parachain_primitives::primitives::{Id as ParaId, Sibling};
use sp_runtime::traits::{AccountIdConversion, Saturating, Zero};

const LOG_TARGET: &str = "runtime::ct-migrator";

pub type BalanceOf<T> =
	<<T as Config>::Currency as Inspect<<T as frame_system::Config>::AccountId>>::Balance;
pub type PortableAccountOf<T> =
	PortableAccount<<T as frame_system::Config>::AccountId, BalanceOf<T>>;
pub type PortableParaInfoOf<T> =
	PortableParaInfo<<T as frame_system::Config>::AccountId, BalanceOf<T>>;
pub type PortableHrmpChannelOf<T> = PortableHrmpChannel<BalanceOf<T>>;

/// Account balance payload in chain-agnostic ("portable") format.
///
/// The relay chain withdraws an account into this shape and the receiving chain integrates it
/// through its regular fungible APIs, so refcounts and events are indistinguishable from locally
/// created state.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableAccount<AccountId, Balance> {
	/// The account address. Sent verbatim; no account-id translation happens for regular
	/// accounts.
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
	/// relay-chain deposits (`paras_registrar`, `hrmp`, `proxy`) are placed. Attribution to the
	/// pallet owning the deposit happens when that pallet's own state migrates.
	#[codec(index = 0)]
	UnnamedReserve,
}

/// Registrar record (`paras_registrar::ParaInfo`) in portable format.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableParaInfo<AccountId, Balance> {
	pub para_id: u32,
	/// The account that placed the registration deposit and manages the para.
	pub manager: AccountId,
	/// The deposit as recorded by the registrar. Reconciled against the balance that actually
	/// arrived held during the accounts stage; never trusted on its own.
	pub deposit: Balance,
	/// Whether the para is locked from manager control.
	pub locked: Option<bool>,
}

/// HRMP channel record in portable format.
///
/// Records only: the channel deposits stay reserved on the para sovereign accounts on the relay
/// chain because sovereign-account translation is not designed yet, and the dynamic message state
/// (`msg_count`, `total_size`, `mqc_head`) is deliberately not migrated.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableHrmpChannel<Balance> {
	pub sender: u32,
	pub recipient: u32,
	pub max_capacity: u32,
	pub max_total_size: u32,
	pub max_message_size: u32,
	pub sender_deposit: Balance,
	pub recipient_deposit: Balance,
}

/// Progress of the migration. Advanced by messages from `pallet-rc2-migrator`.
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
pub enum MigrationStage {
	#[default]
	Pending,
	DataMigrationOngoing,
	MigrationDone,
}

impl MigrationStage {
	pub fn is_finished(&self) -> bool {
		matches!(self, Self::MigrationDone)
	}

	pub fn is_ongoing(&self) -> bool {
		matches!(self, Self::DataMigrationOngoing)
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Native currency. Migrated balances are minted here; migrated reserves land as holds.
		type Currency: Mutate<Self::AccountId>
			+ MutateHold<Self::AccountId, Reason = Self::RuntimeHoldReason>;

		/// The overarching hold reason type.
		///
		/// The `From<PortableHoldReason>` bound is where the runtime declares what each migrated
		/// relay-chain hold becomes locally.
		type RuntimeHoldReason: From<HoldReason> + From<PortableHoldReason>;
	}

	#[pallet::composite_enum]
	pub enum HoldReason {
		/// Balance that was reserved on the relay chain.
		///
		/// Held under this generic reason until the pallet owning the deposit migrates its state
		/// and re-attributes the hold to its own reason.
		#[codec(index = 0)]
		RcMigratedReserve,
		/// A parachain registration deposit migrated from the relay chain.
		///
		/// Placeholder reason: moves to the future registrar pallet's own `HoldReason` when that
		/// pallet lands on this chain.
		#[codec(index = 1)]
		RegistrarDeposit,
		/// An HRMP channel deposit migrated from the relay chain, held on the sibling sovereign
		/// account of the depositing para.
		///
		/// Placeholder reason like [`Self::RegistrarDeposit`], for the future HRMP pallet.
		#[codec(index = 2)]
		HrmpDeposit,
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::storage]
	pub type CtMigrationStage<T: Config> = StorageValue<_, MigrationStage, ValueQuery>;

	/// Accounts that failed to integrate, parked verbatim for manual recovery.
	///
	/// The batch call never fails on a single bad account: the failed account is rolled back,
	/// stored here and the rest of the batch continues.
	#[pallet::storage]
	pub type FailedAccounts<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, PortableAccountOf<T>, OptionQuery>;

	/// Total balance minted on this chain by the accounts stage.
	///
	/// Reconciled against the relay chain's burned total in `finish_migration`.
	#[pallet::storage]
	pub type CtMintedTotal<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	// NOTE: The storage items below are DUMMY stand-ins for the future registrar/HRMP pallets on
	// this chain, which do not exist yet. They hold the migrated records verbatim so the
	// migration pipeline is exercisable end to end; the real pallets replace each map and the
	// `do_receive_*` arm that fills it, nothing else.

	/// Dummy stand-in for the future registrar pallet's `Paras` storage.
	#[pallet::storage]
	pub type RcParas<T: Config> =
		StorageMap<_, Twox64Concat, u32, PortableParaInfoOf<T>, OptionQuery>;

	/// Dummy stand-in for the future registrar pallet's `NextFreeParaId`.
	#[pallet::storage]
	pub type RcNextFreeParaId<T: Config> = StorageValue<_, u32, OptionQuery>;

	/// Dummy stand-in for the future HRMP pallet's `HrmpChannels`, keyed by `(sender, recipient)`.
	#[pallet::storage]
	pub type RcHrmpChannels<T: Config> =
		StorageMap<_, Twox64Concat, (u32, u32), PortableHrmpChannelOf<T>, OptionQuery>;

	/// Registrar records that failed to integrate, parked verbatim for manual recovery.
	#[pallet::storage]
	pub type FailedParas<T: Config> =
		StorageMap<_, Twox64Concat, u32, PortableParaInfoOf<T>, OptionQuery>;

	/// HRMP channel records that failed to integrate, parked verbatim for manual recovery.
	#[pallet::storage]
	pub type FailedHrmpChannels<T: Config> =
		StorageMap<_, Twox64Concat, (u32, u32), PortableHrmpChannelOf<T>, OptionQuery>;

	/// Per-para shortfall between the registrar-recorded deposit and what actually arrived held.
	///
	/// Reconciliation rule: re-attribute `min(recorded, held)`, park the difference here — the
	/// migration never invents balance for deposit records that were not backed by a reserve on
	/// the relay chain (a known on-chain anomaly).
	#[pallet::storage]
	pub type ParkedDepositShortfalls<T: Config> =
		StorageMap<_, Twox64Concat, u32, BalanceOf<T>, OptionQuery>;

	/// Total re-attributed from `RcMigratedReserve` to `RegistrarDeposit` holds.
	#[pallet::storage]
	pub type ReattributedDeposits<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// Total re-attributed from `RcMigratedReserve` to `HrmpDeposit` holds.
	#[pallet::storage]
	pub type ReattributedHrmpDeposits<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// Per-(channel, side) shortfall between the recorded HRMP deposit and what arrived held on
	/// the sibling sovereign. Same reconciliation rule as registrar deposits: `min(recorded,
	/// held)`, difference parked here, never invented.
	#[pallet::storage]
	pub type ParkedHrmpShortfalls<T: Config> =
		StorageMap<_, Twox64Concat, (u32, u32, bool), BalanceOf<T>, OptionQuery>;

	#[pallet::error]
	pub enum Error<T> {
		/// Failed to integrate a migrated account.
		FailedToProcessAccount,
		/// Failed to integrate a migrated registrar record.
		FailedToProcessPara,
		/// Failed to flip a migrated reserve to its attributed hold reason.
		FailedToReattribute,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		StageTransition {
			old: MigrationStage,
			new: MigrationStage,
		},
		/// A batch of migrated accounts was processed.
		AccountsReceived {
			count_good: u32,
			count_bad: u32,
		},
		/// A batch of migrated registrar records was processed.
		RegistrarReceived {
			count_good: u32,
			count_bad: u32,
		},
		/// A registrar deposit could not be fully re-attributed; the shortfall is parked.
		DepositShortfallParked {
			para_id: u32,
			shortfall: BalanceOf<T>,
		},
		/// A batch of migrated HRMP channel records was processed.
		HrmpReceived {
			count: u32,
		},
		/// An HRMP deposit could not be fully re-attributed; the shortfall is parked.
		HrmpShortfallParked {
			sender: u32,
			recipient: u32,
			shortfall: BalanceOf<T>,
		},
		/// The relay chain signalled that all data has been sent.
		MigrationFinished {
			rc_kept: BalanceOf<T>,
			rc_migrated: BalanceOf<T>,
			ct_minted: BalanceOf<T>,
		},
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Receive a batch of accounts migrated from the relay chain.
		///
		/// Dispatched by `pallet-rc2-migrator` via XCM `Transact` with `OriginKind::Superuser`;
		/// the relay chain location converts to Root here.
		///
		/// Weight is a placeholder until the migrator pallets get benchmarks; the payload is
		/// bounded by the sender's batch limits.
		#[pallet::call_index(0)]
		#[pallet::weight(
			T::DbWeight::get().reads_writes(4, 4).saturating_mul(accounts.len() as u64)
		)]
		pub fn receive_accounts(
			origin: OriginFor<T>,
			accounts: Vec<PortableAccountOf<T>>,
		) -> DispatchResult {
			ensure_root(origin)?;

			Self::do_receive_accounts(accounts);
			Ok(())
		}

		/// Receive a batch of registrar records migrated from the relay chain.
		///
		/// Stores each record and re-attributes the manager's migrated reserve to a
		/// `RegistrarDeposit` hold. `next_free_para_id` is carried by the stage-init message only.
		#[pallet::call_index(1)]
		#[pallet::weight(
			T::DbWeight::get().reads_writes(6, 6).saturating_mul((paras.len() as u64).max(1))
		)]
		pub fn receive_registrar(
			origin: OriginFor<T>,
			paras: Vec<PortableParaInfoOf<T>>,
			next_free_para_id: Option<u32>,
		) -> DispatchResult {
			ensure_root(origin)?;

			Self::do_receive_registrar(paras, next_free_para_id);
			Ok(())
		}

		/// Receive a batch of HRMP channel records migrated from the relay chain.
		#[pallet::call_index(2)]
		#[pallet::weight(
			T::DbWeight::get().reads_writes(2, 2).saturating_mul((channels.len() as u64).max(1))
		)]
		pub fn receive_hrmp(
			origin: OriginFor<T>,
			channels: Vec<PortableHrmpChannelOf<T>>,
		) -> DispatchResult {
			ensure_root(origin)?;

			Self::do_receive_hrmp(channels);
			Ok(())
		}

		/// The relay chain signals that all data has been sent.
		///
		/// Carries the relay-side balance bookkeeping so this chain can reconcile what it minted
		/// against what the relay chain burned. A mismatch is loudly reported, never hidden: the
		/// stage still advances so the cool-off verification can inspect the discrepancy.
		#[pallet::call_index(3)]
		#[pallet::weight(T::DbWeight::get().reads_writes(3, 2))]
		pub fn finish_migration(
			origin: OriginFor<T>,
			rc_kept: BalanceOf<T>,
			rc_migrated: BalanceOf<T>,
		) -> DispatchResult {
			ensure_root(origin)?;

			let ct_minted = CtMintedTotal::<T>::get();
			if ct_minted != rc_migrated {
				log::error!(
					target: LOG_TARGET,
					"Minted/burned mismatch: RC burned {rc_migrated:?}, CT minted {ct_minted:?}"
				);
			}
			Self::deposit_event(Event::MigrationFinished { rc_kept, rc_migrated, ct_minted });
			Self::transition(MigrationStage::MigrationDone);
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		fn do_receive_accounts(accounts: Vec<PortableAccountOf<T>>) {
			let stage = CtMigrationStage::<T>::get();
			if stage == MigrationStage::Pending {
				Self::transition(MigrationStage::DataMigrationOngoing);
			}

			let (mut count_good, mut count_bad) = (0, 0);
			for account in accounts {
				// Each account integrates in its own transaction so one bad account cannot
				// poison the batch.
				let res =
					with_transaction_opaque_err::<(), Error<T>, _>(
						|| match Self::do_receive_account(&account) {
							Ok(()) => TransactionOutcome::Commit(Ok(())),
							Err(e) => TransactionOutcome::Rollback(Err(e)),
						},
					)
					.expect("Always returning Ok; qed");

				if let Err(e) = res {
					count_bad += 1;
					log::error!(
						target: LOG_TARGET,
						"Failed to integrate account {:?}: {e:?}; parking it",
						account.who,
					);
					FailedAccounts::<T>::insert(account.who.clone(), account);
				} else {
					count_good += 1;
				}
			}

			Self::deposit_event(Event::AccountsReceived { count_good, count_bad });
		}

		fn do_receive_account(account: &PortableAccountOf<T>) -> Result<(), Error<T>> {
			let who = &account.who;
			let held: BalanceOf<T> = account
				.holds
				.iter()
				.fold(Zero::zero(), |acc: BalanceOf<T>, hold| acc.saturating_add(hold.amount));
			let total = account.free.saturating_add(held);

			// Accounts whose incoming free balance cannot provide the existential deposit get a
			// provider reference so the mint and hold below cannot fail or dust the account.
			if frame_system::Pallet::<T>::providers(who).is_zero() &&
				T::Currency::balance(who).saturating_add(account.free) <
					T::Currency::minimum_balance()
			{
				frame_system::Pallet::<T>::inc_providers(who);
			}

			let minted = T::Currency::mint_into(who, total)
				.map_err(|_| Error::<T>::FailedToProcessAccount)?;
			defensive_assert!(minted == total, "minted what the relay chain burned");
			CtMintedTotal::<T>::mutate(|t| *t = t.saturating_add(minted));

			for hold in &account.holds {
				T::Currency::hold(&hold.reason.into(), who, hold.amount)
					.map_err(|_| Error::<T>::FailedToProcessAccount)?;
			}

			Ok(())
		}

		fn do_receive_registrar(paras: Vec<PortableParaInfoOf<T>>, next_free: Option<u32>) {
			if let Some(id) = next_free {
				RcNextFreeParaId::<T>::put(id);
			}

			let (mut count_good, mut count_bad) = (0, 0);
			for para in paras {
				// Each record integrates in its own transaction so one bad record cannot poison
				// the batch.
				let res = with_transaction_opaque_err::<(), Error<T>, _>(|| {
					match Self::do_receive_para(&para) {
						Ok(()) => TransactionOutcome::Commit(Ok(())),
						Err(e) => TransactionOutcome::Rollback(Err(e)),
					}
				})
				.expect("Always returning Ok; qed");

				if let Err(e) = res {
					count_bad += 1;
					log::error!(
						target: LOG_TARGET,
						"Failed to integrate para {}: {e:?}; parking it",
						para.para_id,
					);
					FailedParas::<T>::insert(para.para_id, para);
				} else {
					count_good += 1;
				}
			}

			Self::deposit_event(Event::RegistrarReceived { count_good, count_bad });
		}

		/// Flip `min(wanted, held-as-RcMigratedReserve)` on `who` to the attributed reason `to`.
		///
		/// Returns `(attributed, shortfall)`. A shortfall means the recorded deposit was not
		/// (fully) backed by a live reserve on the relay chain, or the depositor's account was
		/// kept there; callers park it, it is never minted.
		fn reattribute_hold(
			who: &T::AccountId,
			wanted: BalanceOf<T>,
			to: HoldReason,
		) -> Result<(BalanceOf<T>, BalanceOf<T>), Error<T>> {
			let rc_reason: T::RuntimeHoldReason = HoldReason::RcMigratedReserve.into();
			let held = T::Currency::balance_on_hold(&rc_reason, who);
			let attribute = wanted.min(held);

			if !attribute.is_zero() {
				T::Currency::release(&rc_reason, who, attribute, Precision::Exact)
					.map_err(|_| Error::<T>::FailedToReattribute)?;
				T::Currency::hold(&to.into(), who, attribute)
					.map_err(|_| Error::<T>::FailedToReattribute)?;
			}
			Ok((attribute, wanted.saturating_sub(attribute)))
		}

		fn do_receive_para(para: &PortableParaInfoOf<T>) -> Result<(), Error<T>> {
			let (attributed, shortfall) = Self::reattribute_hold(
				&para.manager,
				para.deposit,
				HoldReason::RegistrarDeposit,
			)?;
			ReattributedDeposits::<T>::mutate(|t| *t = t.saturating_add(attributed));
			if !shortfall.is_zero() {
				ParkedDepositShortfalls::<T>::insert(para.para_id, shortfall);
				Self::deposit_event(Event::DepositShortfallParked {
					para_id: para.para_id,
					shortfall,
				});
			}

			RcParas::<T>::insert(para.para_id, para.clone());
			Ok(())
		}

		fn do_receive_hrmp(channels: Vec<PortableHrmpChannelOf<T>>) {
			let (mut count_good, mut count_bad) = (0, 0);
			for channel in channels {
				// Each channel integrates in its own transaction so one bad record cannot poison
				// the batch.
				let res = with_transaction_opaque_err::<(), Error<T>, _>(|| {
					match Self::do_receive_channel(&channel) {
						Ok(()) => TransactionOutcome::Commit(Ok(())),
						Err(e) => TransactionOutcome::Rollback(Err(e)),
					}
				})
				.expect("Always returning Ok; qed");

				if let Err(e) = res {
					count_bad += 1;
					log::error!(
						target: LOG_TARGET,
						"Failed to integrate channel {}->{}: {e:?}; parking it",
						channel.sender, channel.recipient,
					);
					FailedHrmpChannels::<T>::insert((channel.sender, channel.recipient), channel);
				} else {
					count_good += 1;
				}
			}
			Self::deposit_event(Event::HrmpReceived { count: count_good });
			if count_bad > 0 {
				log::error!(target: LOG_TARGET, "{count_bad} HRMP channels parked");
			}
		}

		fn do_receive_channel(channel: &PortableHrmpChannelOf<T>) -> Result<(), Error<T>> {
			// The deposits arrived as holds on the *sibling sovereign* accounts of the two paras
			// (the accounts stage translates child sovereigns); re-attribute each side.
			for (para, wanted, side) in [
				(channel.sender, channel.sender_deposit, true),
				(channel.recipient, channel.recipient_deposit, false),
			] {
				let sovereign: T::AccountId =
					Sibling::from(ParaId::from(para)).into_account_truncating();
				let (attributed, shortfall) =
					Self::reattribute_hold(&sovereign, wanted, HoldReason::HrmpDeposit)?;
				ReattributedHrmpDeposits::<T>::mutate(|t| *t = t.saturating_add(attributed));
				if !shortfall.is_zero() {
					ParkedHrmpShortfalls::<T>::insert(
						(channel.sender, channel.recipient, side),
						shortfall,
					);
					Self::deposit_event(Event::HrmpShortfallParked {
						sender: channel.sender,
						recipient: channel.recipient,
						shortfall,
					});
				}
			}

			RcHrmpChannels::<T>::insert((channel.sender, channel.recipient), channel.clone());
			Ok(())
		}

		fn transition(new: MigrationStage) {
			let old = CtMigrationStage::<T>::get();
			CtMigrationStage::<T>::put(new.clone());
			log::info!(target: LOG_TARGET, "Stage transition: {old:?} -> {new:?}");
			Self::deposit_event(Event::StageTransition { old, new });
		}
	}
}
