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

pub mod accounts;

pub use pallet::*;

use accounts::{ExpectedReserve, MigratedBalances};
use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::BlockNumberFor;
use migrator_types::PortableProxyType;
use polkadot_parachain_primitives::primitives::{HrmpChannelId, Id as ParaId};
use polkadot_runtime_common::paras_registrar;
use sp_runtime::AccountId32;

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
		// The `Currency` equalities pin every recorded deposit to the native u128 balance. The
		// `ProxyType` bound is where the runtime declares which proxy permissions travel to the
		// Coretime chain; untranslatable ones stay here.
		+ paras_registrar::Config<Currency = pallet_balances::Pallet<Self>>
		+ runtime_parachains::hrmp::Config
		+ pallet_multisig::Config<Currency = pallet_balances::Pallet<Self>>
		+ pallet_proxy::Config<
			Currency = pallet_balances::Pallet<Self>,
			ProxyType: TryInto<PortableProxyType>,
		>
		// Preimage deposits are released before the accounts stage runs: they are named holds,
		// and `can_migrate` refuses any account that has one. The bound is on the pallet rather
		// than a `StorePreimage` seam because the deposits have to be *enumerated*, which only
		// the pallet's storage can do.
		+ pallet_preimage::Config
	{
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Working buffer of free balance that follows a migrated deposit to the Coretime chain,
		/// so deposit owners can pay fees and future deposits there without a teleport first.
		#[pallet::constant]
		type CtFreeBuffer: Get<u128>;

		/// Asset Hub's existential deposit. Free balance below this cannot be teleported into a
		/// fresh account; such dust follows the deposit to the Coretime chain instead.
		#[pallet::constant]
		type AhExistentialDeposit: Get<u128>;
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

	/// What each account's reserved balance is expected to be made of, built from the owning
	/// pallets' recorded deposit fields before any account is withdrawn. The recorded fields are
	/// the routing source of truth; the anonymous reserves are only trusted up to these amounts,
	/// and anything beyond them travels as an unattributed hold, parked at the destination.
	///
	/// One record per account rather than one map per kind: the three amounts are always built
	/// together and always read together in the withdrawal split.
	#[pallet::storage]
	pub type ExpectedReserves<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, ExpectedReserve, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		StageTransition {
			old: MigrationStageOf<T>,
			new: MigrationStageOf<T>,
		},
		/// An account carried reserve that no pallet's deposit records account for. It travels to
		/// the Coretime chain under its own hold reason and stays parked there for investigation.
		UnattributedReserve {
			who: AccountId32,
			amount: u128,
		},
		/// A deposit whose purpose ends with this chain was released; it travels to Asset Hub as
		/// free balance.
		DepositRefunded {
			who: AccountId32,
			amount: u128,
		},
		/// An account that a consumer reference forbids reaping (session keys being the known
		/// case) was drained to a zero-balance shell; the balance travels like any other
		/// account's.
		AccountShellDrained {
			who: AccountId32,
			amount: u128,
		},
	}
}
