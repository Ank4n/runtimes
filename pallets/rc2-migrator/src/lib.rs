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

pub mod sweep;

pub use pallet::*;

use alloc::vec::Vec;
use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::BlockNumberFor;
use polkadot_parachain_primitives::primitives::{HrmpChannelId, Id as ParaId};
use sp_runtime::AccountId32;
use sweep::MigratedBalances;

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

		/// Leftover module pots to empty in the `Sweep` stage (e.g. the old treasury pot).
		/// Their full balance teleports to `SweepBeneficiary`.
		type SweepAccounts: Get<Vec<AccountId32>>;

		/// Where swept pots and reaped dust land on Asset Hub — the treasury / DAP buffer
		/// account designated by governance.
		type SweepBeneficiary: Get<AccountId32>;
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
		/// Zero-balance records held alive only by stale provider references were reaped.
		HusksReaped {
			count: u32,
		},
	}
}
