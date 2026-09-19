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
//! Drives the migration stage machine: drains legacy `paras_registrar` and `hrmp` state together
//! with their deposits and sends everything to the counterpart `pallet-ct-migrator` over XCM.
//! Temporary pallet; removed once the migration is complete.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod multisig;

pub use pallet::*;

use alloc::vec::Vec;
use frame_support::{dispatch::GetDispatchInfo, pallet_prelude::*};
use frame_system::pallet_prelude::BlockNumberFor;
use polkadot_parachain_primitives::primitives::{HrmpChannelId, Id as ParaId};
use sp_runtime::{traits::Dispatchable, AccountId32};

pub type MigrationStageOf<T> = MigrationStage<BlockNumberFor<T>>;

/// Progress of the migration. Advanced by `on_initialize`.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, Default, PartialEq, Eq, Debug, TypeInfo)]
pub enum MigrationStage<BlockNumber> {
	#[default]
	Pending,
	Scheduled {
		start: BlockNumber,
	},
	Paused,
	/// Waiting for the Coretime chain to confirm that it is ready to receive data.
	WaitingForCt,
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
	/// All data sent; waiting for manual verification before finishing.
	CoolOff {
		end_at: BlockNumber,
	},
	MigrationDone,
}

impl<BlockNumber> MigrationStage<BlockNumber> {
	pub fn is_finished(&self) -> bool {
		matches!(self, Self::MigrationDone)
	}

	pub fn is_ongoing(&self) -> bool {
		!matches!(self, Self::Pending | Self::Scheduled { .. } | Self::MigrationDone)
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

		/// Calls the manager multisig may dispatch once it reaches its threshold.
		type RuntimeCall: Parameter
			+ Dispatchable<RuntimeOrigin = <Self as frame_system::Config>::RuntimeOrigin>
			+ GetDispatchInfo;

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

	/// The multisig members that voted to execute a specific call.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type ManagerMultisigs<T: Config> =
		StorageMap<_, Twox64Concat, <T as Config>::RuntimeCall, Vec<AccountId32>, ValueQuery>;

	/// The current round of the multisig voting. Votes are only valid for the current round.
	#[pallet::storage]
	pub type ManagerMultisigRound<T: Config> = StorageValue<_, u32, ValueQuery>;

	/// How often each member voted in the current round. Cleared at the end of each round.
	#[pallet::storage]
	pub type ManagerVotesInCurrentRound<T: Config> =
		StorageMap<_, Blake2_128Concat, AccountId32, u32, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		StageTransition {
			old: MigrationStageOf<T>,
			new: MigrationStageOf<T>,
		},
		/// The manager multisig dispatched a call.
		ManagerMultisigDispatched {
			res: DispatchResult,
		},
		/// The manager multisig received a vote.
		ManagerMultisigVoted {
			votes: u32,
		},
	}
}
