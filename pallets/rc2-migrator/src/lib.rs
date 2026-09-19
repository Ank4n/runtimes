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

pub mod ti_correction;

pub use pallet::*;

use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::BlockNumberFor;
use polkadot_parachain_primitives::primitives::{HrmpChannelId, Id as ParaId};
use sp_runtime::AccountId32;
use ti_correction::MigratedBalances;

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
	pub trait Config:
		frame_system::Config<
			AccountId = AccountId32,
			AccountData = pallet_balances::AccountData<u128>,
		> + pallet_balances::Config<Balance = u128>
	{
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The audited amount of total issuance that no account holds ("phantom issuance"),
		/// burned by the `TiCorrection` stage at the end of the migration.
		///
		/// Governance-legible tunable: measured off-chain ahead of the migration
		/// (`balance_census` prints the exact planck value) and pinned here. The stage burns
		/// `min(this, measured-on-chain)` — anything unaccounted beyond it is left for
		/// investigation, and a measured value *below* it is reported as an anomaly; the stage
		/// never burns issuance that an account actually holds.
		#[pallet::constant]
		type TiCorrection: Get<u128>;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::storage]
	#[pallet::unbounded]
	pub type RcMigrationStage<T: Config> = StorageValue<_, MigrationStageOf<T>, ValueQuery>;

	/// Balance kept on the relay chain versus migrated away. Seeded and maintained by the
	/// accounts stage; the conservation ledger every later stage keeps exact.
	#[pallet::storage]
	pub type RcMigratedBalance<T: Config> = StorageValue<_, MigratedBalances, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		StageTransition {
			old: MigrationStageOf<T>,
			new: MigrationStageOf<T>,
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
	}
}
