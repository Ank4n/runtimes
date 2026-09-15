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

//! Relay-chain side of the AHM v2 migration.
//!
//! Drives the migration stage machine: drains legacy `paras_registrar` and `hrmp` state together
//! with their deposits and sends everything to the counterpart `pallet-ct-migrator` over XCM.
//! Temporary pallet; removed once the migration is complete.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use pallet::*;

use alloc::{vec, vec::Vec};
use frame_support::{
	dispatch::GetDispatchInfo,
	pallet_prelude::*,
	traits::{EnsureOrigin, Time},
	PalletId,
};
use frame_system::pallet_prelude::*;
use polkadot_parachain_primitives::primitives::{HrmpChannelId, Id as ParaId};
use sp_runtime::{
	traits::{AccountIdConversion, Dispatchable, IdentifyAccount, Verify},
	AccountId32, MultiSignature, MultiSigner,
};
use xcm::prelude::*;

const LOG_TARGET: &str = "runtime::rc2-migrator";

/// Wall-clock type the schedule is expressed in.
pub type MomentOf<T> = <<T as Config>::TimeProvider as Time>::Moment;
pub type MigrationStageOf<T> =
	MigrationStage<<T as frame_system::Config>::AccountId, BlockNumberFor<T>, MomentOf<T>>;

/// Progress of the migration. Advanced by `on_initialize`, except where noted.
///
/// Variants are in the order the migration progresses through them.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, Default, PartialEq, Eq, Debug, TypeInfo)]
pub enum MigrationStage<AccountId, BlockNumber, Moment> {
	/// Nothing has been scheduled; `on_initialize` does no work.
	#[default]
	Pending,
	/// Scheduled to begin at the first block whose predecessor's timestamp is at or past `start`.
	Scheduled {
		start: Moment,
	},
	/// Halts the machine without ending the migration. Entered and left only via
	/// [`Pallet::force_set_stage`].
	Paused,
	/// Waiting for the Coretime chain to confirm that it is ready to receive data.
	WaitingForCt,
	/// Account balances, their reserves, and the holds those reserves become.
	AccountsInit,
	AccountsOngoing {
		last_key: Option<AccountId>,
	},
	AccountsDone,
	/// Proxy definitions whose permissions have meaning on the Coretime chain.
	///
	/// Runs before the registrar so classification can still read the un-drained `Paras` map.
	ProxyInit,
	ProxyOngoing {
		last_key: Option<AccountId>,
	},
	ProxyDone,
	/// `paras_registrar` records and their deposits.
	RegistrarInit,
	RegistrarOngoing {
		last_key: Option<ParaId>,
	},
	RegistrarDone,
	/// HRMP channels, pending open requests, and their deposits.
	HrmpInit,
	HrmpOngoing {
		last_key: Option<HrmpChannelId>,
	},
	HrmpDone,
	/// Empty the pots whose balance has no owning account to migrate it with, such as the
	/// treasury's.
	Sweep,
	/// Reap the accounts left below the existential deposit, and the zero-balance husks.
	///
	/// Runs after the accounts stage because most of what it reaps does not exist until the
	/// earlier stages have run, and because [`Self::TiCorrection`] reads its output: burning the
	/// issuance no account holds is only safe once the husks are gone.
	SweepDust {
		last_key: Option<AccountId>,
	},
	/// Burn the audited issuance that no account holds.
	TiCorrection,
	/// All data sent; waiting for manual verification before finishing.
	CoolOff {
		end_at: BlockNumber,
	},
	MigrationDone,
}

impl<AccountId, BlockNumber, Moment> MigrationStage<AccountId, BlockNumber, Moment> {
	pub fn is_finished(&self) -> bool {
		matches!(self, Self::MigrationDone)
	}

	/// Whether the machine is between its start and its end.
	pub fn is_ongoing(&self) -> bool {
		!matches!(self, Self::Pending | Self::Scheduled { .. } | Self::MigrationDone)
	}

	/// Whether the machine has left [`Self::Pending`]/[`Self::Scheduled`]. Stays true after
	/// [`Self::MigrationDone`].
	pub fn has_started(&self) -> bool {
		self.is_ongoing() || self.is_finished()
	}
}

/// `CtMigrator`'s pallet index in the Coretime (receiver) chain.
pub const CT_MIGRATOR_PALLET_INDEX: u8 = 100;

/// Calls on the Coretime chain, as this chain must encode them.
#[derive(Encode, Decode, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CtRuntimeCall {
	CtMigrator(CtMigratorCall) = CT_MIGRATOR_PALLET_INDEX,
}

/// Indices are the `#[pallet::call_index]`es in `pallet-ct-migrator`.
#[derive(Encode, Decode, PartialEq, Eq, Debug)]
pub enum CtMigratorCall {
	#[codec(index = 0)]
	StartMigration,
	#[codec(index = 1)]
	FinishMigration,
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Router for XCM messages to the Coretime chain.
		type SendXcm: SendXcm;

		/// Para id of the Coretime chain.
		type CtParaId: Get<u32>;

		/// Wall clock the schedule is compared against, so a schedule set weeks ahead does not
		/// drift with block times.
		type TimeProvider: Time;

		/// The origin that the Coretime chain's messages dispatch with on this chain.
		type CtOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// How long the machine parks in [`MigrationStage::CoolOff`] before finishing.
		type CoolOffPeriod: Get<BlockNumberFor<Self>>;

		/// Calls the manager multisig may dispatch once it reaches its threshold.
		type RuntimeCall: Parameter
			+ Dispatchable<RuntimeOrigin = <Self as frame_system::Config>::RuntimeOrigin>
			+ GetDispatchInfo;

		/// Governance. Appoints the [`Manager`], and may drive the migration itself.
		type AdminOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

		/// Members of a multisig that can submit unsigned txs and act as the manager.
		type MultisigMembers: Get<Vec<AccountId32>>;

		/// Threshold of `MultisigMembers`.
		type MultisigThreshold: Get<u32>;

		/// Limit the number of votes of each participant per round.
		type MultisigMaxVotesPerRound: Get<u32>;

		/// Round the vote counter starts at. Must differ per network.
		type MultisigStartRound: Get<u32>;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::storage]
	#[pallet::unbounded]
	pub type RcMigrationStage<T: Config> = StorageValue<_, MigrationStageOf<T>, ValueQuery>;

	/// An optional account id of a manager.
	///
	/// The manager has privileges similar to [`Config::AdminOrigin`] except that it cannot set the
	/// manager account id via `set_manager`.
	#[pallet::storage]
	pub type Manager<T: Config> = StorageValue<_, T::AccountId, OptionQuery>;

	/// The multisig AccountIDs that voted to execute a specific call.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type ManagerMultisigs<T: Config> =
		StorageMap<_, Twox64Concat, <T as Config>::RuntimeCall, Vec<AccountId32>, ValueQuery>;

	/// The current round of the multisig voting.
	///
	/// Votes are only valid for the current round.
	#[pallet::storage]
	pub type ManagerMultisigRound<T: Config> = StorageValue<_, u32, ValueQuery>;

	/// How often each participant voted in the current round.
	///
	/// Will be cleared at the end of each round.
	#[pallet::storage]
	pub type ManagerVotesInCurrentRound<T: Config> =
		StorageMap<_, Blake2_128Concat, AccountId32, u32, ValueQuery>;

	#[pallet::error]
	pub enum Error<T> {
		/// The migration can only be scheduled while it is pending.
		AlreadyScheduled,
		/// The migration cannot be scheduled to start in the past.
		StartInPast,
		/// Readiness was confirmed while the machine was not waiting for it.
		NotWaitingForCt,
		/// Sending an XCM message to the Coretime chain failed.
		XcmSendFailed,
		/// The migration can only be cancelled while it is scheduled.
		NotScheduled,
		/// An account that is referenced cannot be preserved through the migration.
		AccountReferenced,
		/// The unsigned multisig vote did not validate.
		UnsignedValidationFailed,
		/// The vote carries a round that is no longer open.
		RoundStale,
		/// The member has used up its votes for this round.
		MaxVotesPerRound,
		/// The member has already voted for this call in this round.
		DuplicateVote,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		StageTransition {
			old: MigrationStageOf<T>,
			new: MigrationStageOf<T>,
		},
		/// The manager account id was set.
		ManagerSet {
			old: Option<T::AccountId>,
			new: Option<T::AccountId>,
		},
		/// The manager multisig dispatched something.
		ManagerMultisigDispatched {
			res: DispatchResult,
		},
		/// The manager multisig received a vote.
		ManagerMultisigVoted {
			votes: u32,
		},
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(now: BlockNumberFor<T>) -> Weight {
			Self::progress_migration(now)
		}

		fn on_runtime_upgrade() -> Weight {
			// A vote is signed over (who, call, round) and nothing else, so two chains sitting at
			// the same round accept each other's signatures. Seeding different starting rounds per
			// chain is what v1 did about it, and the runtime picks the number.
			if !ManagerMultisigRound::<T>::exists() {
				ManagerMultisigRound::<T>::put(T::MultisigStartRound::get());
			}

			T::DbWeight::get().reads_writes(1, 1)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Schedule the migration to begin at `start`.
		///
		/// The only way out of [`MigrationStage::Pending`] other than `force_set_stage`.
		#[pallet::call_index(0)]
		#[pallet::weight(T::DbWeight::get().reads_writes(2, 1))]
		pub fn schedule_migration(origin: OriginFor<T>, start: MomentOf<T>) -> DispatchResult {
			Self::ensure_admin_or_manager(origin)?;
			ensure!(
				RcMigrationStage::<T>::get() == MigrationStage::Pending,
				Error::<T>::AlreadyScheduled
			);
			ensure!(start > T::TimeProvider::now(), Error::<T>::StartInPast);

			Self::transition(MigrationStage::Scheduled { start });
			Ok(())
		}

		/// Set the migration stage directly.
		///
		/// Root-only escape hatch for a lost message or a stage that needs re-running.
		#[pallet::call_index(1)]
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub fn force_set_stage(origin: OriginFor<T>, stage: MigrationStageOf<T>) -> DispatchResult {
			Self::ensure_admin_or_manager(origin)?;

			Self::transition(stage);
			Ok(())
		}

		/// The Coretime chain confirms that it can receive migrated state.
		///
		/// Sent by `pallet-ct-migrator` in response to [`CtMigratorCall::StartMigration`]. Nothing
		/// is drained before it arrives.
		#[pallet::call_index(2)]
		#[pallet::weight(T::DbWeight::get().reads_writes(2, 1))]
		pub fn ct_ready(origin: OriginFor<T>) -> DispatchResult {
			T::CtOrigin::ensure_origin(origin)?;
			ensure!(
				RcMigrationStage::<T>::get() == MigrationStage::WaitingForCt,
				Error::<T>::NotWaitingForCt
			);

			let end_at = frame_system::Pallet::<T>::block_number() + T::CoolOffPeriod::get();
			Self::transition(MigrationStage::CoolOff { end_at });
			Ok(())
		}

		/// Set the manager account id.
		///
		/// The manager has privileges similar to [`Config::AdminOrigin`] except that it cannot set
		/// the manager account id via this call.
		///
		/// The account must be unreferenced, so that the migration can reap it at the end.
		#[pallet::call_index(3)]
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub fn set_manager(origin: OriginFor<T>, new: Option<T::AccountId>) -> DispatchResult {
			<T as Config>::AdminOrigin::ensure_origin(origin)?;
			if let Some(ref who) = new {
				ensure!(
					frame_system::Pallet::<T>::consumers(who) == 0,
					Error::<T>::AccountReferenced
				);
				// v1 additionally marked the manager `AccountState::Preserve` in its accounts map
				// so the drain would skip it. There is no accounts map here yet; the accounts
				// stage has to read `Manager` and keep it funded until the cool-off reaps it.
			}
			let old = Manager::<T>::get();
			Manager::<T>::set(new.clone());
			Self::deposit_event(Event::ManagerSet { old, new });
			Ok(())
		}

		/// Return the machine to [`MigrationStage::Pending`] so it can be rescheduled.
		#[pallet::call_index(4)]
		#[pallet::weight(T::DbWeight::get().reads_writes(2, 1))]
		pub fn cancel_migration(origin: OriginFor<T>) -> DispatchResult {
			Self::ensure_admin_or_manager(origin)?;
			ensure!(
				matches!(RcMigrationStage::<T>::get(), MigrationStage::Scheduled { .. }),
				Error::<T>::NotScheduled
			);

			Self::transition(MigrationStage::Pending);
			Ok(())
		}

		/// Vote on behalf of any of the members in [`Config::MultisigMembers`].
		///
		/// Unsigned extrinsic, requiring the `payload` to be signed. Members therefore need no
		/// funded account on this chain, which is the point: this chain is being drained.
		///
		/// Upon each call, a new entry is created in `ManagerMultisigs` mapping `payload.call` to
		/// the members that have voted for it. Once [`Config::MultisigThreshold`] is reached the
		/// entire map is deleted and we move on to the next round.
		///
		/// The round system ensures that signatures from an older round cannot be reused.
		#[pallet::call_index(5)]
		#[pallet::weight(Weight::from_parts(10_000_000, 1000))]
		pub fn vote_manager_multisig(
			origin: OriginFor<T>,
			payload: Box<ManagerMultisigVote<T>>,
			sig: MultiSignature,
		) -> DispatchResult {
			ensure_none(origin)?;

			Self::do_validate_unsigned(&payload, &sig)
				.map_err(|_| Error::<T>::UnsignedValidationFailed)?;
			let who = payload.who.clone().into_account();

			ensure!(ManagerMultisigRound::<T>::get() == payload.round, Error::<T>::RoundStale);
			let num_votes = ManagerVotesInCurrentRound::<T>::get(&who);
			ensure!(num_votes < T::MultisigMaxVotesPerRound::get(), Error::<T>::MaxVotesPerRound);
			ManagerVotesInCurrentRound::<T>::insert(&who, num_votes.saturating_add(1));

			let mut votes_for_call = ManagerMultisigs::<T>::get(&payload.call);
			ensure!(!votes_for_call.contains(&who), Error::<T>::DuplicateVote);
			votes_for_call.push(who);

			if votes_for_call.len() >= T::MultisigThreshold::get() as usize {
				let origin: <T as frame_system::Config>::RuntimeOrigin =
					frame_system::RawOrigin::Signed(Self::manager_multisig_id()).into();
				let call = payload.call.clone();
				let res = call.dispatch(origin);
				let _ = ManagerMultisigs::<T>::clear(u32::MAX, None);
				let _ = ManagerVotesInCurrentRound::<T>::clear(u32::MAX, None);

				Self::deposit_event(Event::ManagerMultisigDispatched {
					res: res.map(|_| ()).map_err(|e| e.error),
				});
				ManagerMultisigRound::<T>::mutate(|r| *r += 1);
			} else {
				Self::deposit_event(Event::ManagerMultisigVoted {
					votes: votes_for_call.len() as u32,
				});
				ManagerMultisigs::<T>::insert(payload.call.clone(), votes_for_call);
			}

			Ok(())
		}
	}

	/// One member's vote for `call`, signed offline and submitted by anyone.
	#[derive(
		Encode,
		Decode,
		DecodeWithMemTracking,
		DebugNoBound,
		CloneNoBound,
		PartialEqNoBound,
		EqNoBound,
		TypeInfo,
	)]
	#[scale_info(skip_type_params(T))]
	pub struct ManagerMultisigVote<T: Config> {
		pub who: MultiSigner,
		pub call: <T as Config>::RuntimeCall,
		pub round: u32,
	}

	impl<T: Config> ManagerMultisigVote<T> {
		pub fn new(who: MultiSigner, call: <T as Config>::RuntimeCall, round: u32) -> Self {
			Self { who, call, round }
		}

		/// The bytes a member signs. The wrapper is what wallet `signRaw` prepends.
		pub fn encode_with_bytes_wrapper(&self) -> Vec<u8> {
			(b"<Bytes>", self, b"</Bytes>").encode()
		}
	}

	// v1 shipped this on `ValidateUnsigned`, which is deprecated for removal after April 2027 in
	// favour of `#[pallet::authorize]` + `frame_system::AuthorizeCall`
	// (paritytech/polkadot-sdk#2415). Kept as v1 wrote it; this pallet is deleted after the
	// migration.
	#[allow(deprecated)]
	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;

		fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			if let Call::vote_manager_multisig { payload, sig } = call {
				Self::do_validate_unsigned(payload, sig)
			} else {
				InvalidTransaction::Call.into()
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// The account the manager multisig dispatches as, once it reaches its threshold.
		///
		/// v1 used `rcmigmts`; a distinct id keeps the two migrations' votes from sharing an
		/// origin on a chain that carried both.
		pub fn manager_multisig_id() -> T::AccountId {
			PalletId(*b"rc2migmt").into_account_truncating()
		}

		/// Ensure that the origin is [`Config::AdminOrigin`], or signed by [`Manager`], or by the
		/// manager multisig.
		fn ensure_admin_or_manager(origin: OriginFor<T>) -> DispatchResult {
			if let Ok(account_id) = ensure_signed(origin.clone()) {
				if Manager::<T>::get().is_some_and(|manager_id| manager_id == account_id) {
					return Ok(());
				}
				if account_id == Self::manager_multisig_id() {
					return Ok(());
				}
			}
			<T as Config>::AdminOrigin::ensure_origin(origin)?;
			Ok(())
		}

		fn do_validate_unsigned(
			payload: &ManagerMultisigVote<T>,
			sig: &MultiSignature,
		) -> TransactionValidity {
			let account = payload.who.clone().into_account();

			if !T::MultisigMembers::get().contains(&account) {
				return InvalidTransaction::BadSigner.into();
			}
			if !sig.verify(&payload.encode_with_bytes_wrapper()[..], &account) {
				return InvalidTransaction::BadProof.into();
			}
			if ManagerMultisigRound::<T>::get() != payload.round {
				return InvalidTransaction::Stale.into();
			}
			if ManagerVotesInCurrentRound::<T>::get(&account) >= T::MultisigMaxVotesPerRound::get()
			{
				return InvalidTransaction::Stale.into();
			}

			ValidTransaction::with_tag_prefix("Ahm2Multisig")
				.priority(sp_runtime::traits::Bounded::max_value())
				.and_provides(vec![("ahm2_multi", account).encode()])
				.propagate(true)
				.longevity(30)
				.build()
		}

		/// One block of the stage machine.
		// TODO(ahm-v2): proper benchmark
		fn progress_migration(now: BlockNumberFor<T>) -> Weight {
			match RcMigrationStage::<T>::get() {
				// The scheduled start is compared against the clock, which at `on_initialize` still
				// holds the previous block's timestamp -- so the migration begins on the first
				// block after the one whose timestamp passed `start`.
				MigrationStage::Scheduled { start } if T::TimeProvider::now() >= start => {
					if Self::send_to_ct(CtMigratorCall::StartMigration).is_ok() {
						Self::transition(MigrationStage::WaitingForCt);
					}
					T::DbWeight::get().reads_writes(3, 3)
				},
				MigrationStage::CoolOff { end_at } if now >= end_at => {
					if Self::send_to_ct(CtMigratorCall::FinishMigration).is_ok() {
						Self::transition(MigrationStage::MigrationDone);
					}
					T::DbWeight::get().reads_writes(3, 3)
				},
				_ => T::DbWeight::get().reads(1),
			}
		}

		pub(crate) fn transition(new: MigrationStageOf<T>) {
			let old = RcMigrationStage::<T>::mutate(|stage| core::mem::replace(stage, new.clone()));
			log::info!(target: LOG_TARGET, "Stage transition: {old:?} -> {new:?}");
			Self::deposit_event(Event::StageTransition { old, new });
		}

		/// Send a `pallet-ct-migrator` call to the Coretime chain.
		fn send_to_ct(call: CtMigratorCall) -> Result<(), Error<T>> {
			let call = CtRuntimeCall::CtMigrator(call);
			// `Superuser` converts to Root on the Coretime chain; the receiving calls check for
			// Root.
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
