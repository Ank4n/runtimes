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

pub mod proxy;

pub use pallet::*;

use frame_support::{
	pallet_prelude::*,
	traits::fungible::{Mutate, MutateHold},
};
use migrator_types::PortableProxyType;
use proxy::PortableProxyOf;

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
	pub trait Config:
		frame_system::Config
		// Migrated proxy delegations are written into the real proxy pallet so keyless (pure)
		// delegators keep control here. The `ProxyType` bound is where the runtime declares
		// what each portable permission becomes locally.
		+ pallet_proxy::Config<ProxyType: From<PortableProxyType>>
	{
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Native currency. Migrated relay-chain deposits sit here as holds.
		type Currency: Mutate<Self::AccountId>
			+ MutateHold<Self::AccountId, Reason = Self::RuntimeHoldReason>;

		/// The overarching hold reason type.
		type RuntimeHoldReason: From<HoldReason>;

		/// How many of this chain's blocks fit in one relay-chain block's time. Used to convert
		/// migrated proxy delays (relay: 6s blocks; this chain: 12s → ratio 2).
		#[pallet::constant]
		type RcBlockTimeRatio: Get<u32>;
	}

	#[pallet::composite_enum]
	pub enum HoldReason {
		/// A relay-chain proxy deposit whose definitions travel here. Released when they arrive:
		/// the recreated entry is re-reserved at this chain's rates and the rest becomes free.
		#[codec(index = 1)]
		ProxyDeposit,
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::storage]
	pub type CtMigrationStage<T: Config> = StorageValue<_, MigrationStage, ValueQuery>;

	/// Migrated proxy sets that failed to integrate, parked verbatim for recovery.
	#[pallet::storage]
	pub type FailedProxies<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, PortableProxyOf<T>, OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		StageTransition {
			old: MigrationStage,
			new: MigrationStage,
		},
		/// A batch of migrated proxy sets was processed.
		ProxiesReceived {
			count_good: u32,
			count_bad: u32,
		},
	}
}
