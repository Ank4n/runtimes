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

#![cfg_attr(not(feature = "std"), no_std)]

pub mod accounts;

pub use pallet::*;

use accounts::{BalanceOf, PortableAccountOf};
use frame_support::{
	pallet_prelude::*,
	traits::fungible::{Mutate, MutateHold},
};
use migrator_types::PortableHoldReason;

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
		/// A relay-chain proxy deposit whose definitions travel here. Released when they arrive:
		/// the recreated entry is re-reserved at this chain's rates and the rest becomes free.
		#[codec(index = 1)]
		ProxyDeposit,
		/// Relay-chain reserve that no pallet's deposit records accounted for. Parked here for
		/// investigation — nothing was allowed to stay behind on the relay chain — and never
		/// re-attributed by any stage.
		#[codec(index = 2)]
		UnattributedReserve,
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::storage]
	pub type CtMigrationStage<T: Config> = StorageValue<_, MigrationStage, ValueQuery>;

	/// Accounts that failed to integrate, parked verbatim for recovery after the migration.
	///
	/// A batch never fails on a single bad account: it is rolled back, stored here, and the rest
	/// of the batch continues. Each entry is balance the relay chain burned and this chain never
	/// minted, so this map is both the record of the gap and the data needed to close it.
	#[pallet::storage]
	pub type FailedAccounts<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, PortableAccountOf<T>, OptionQuery>;

	/// Total balance minted on this chain by the accounts stage. Reconciled against the relay
	/// chain's burned total once the migration ends.
	#[pallet::storage]
	pub type CtMintedTotal<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
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
	}
}
