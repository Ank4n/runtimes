// Copyright (C) Polkadot Fellows.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot. If not, see <http://www.gnu.org/licenses/>.

//! AHM v2 migration wiring: the relay-chain side of moving account, proxy, registrar and HRMP
//! state to the Coretime chain.
//!
//! Compiled only with the `ahm-v2` feature, which released runtimes do not enable. The
//! integration tests turn it on to drive the real runtime.

use crate::{
	xcm_config::{CoretimeLocation, XcmRouter},
	AccountId, BrokerId, Runtime, RuntimeEvent, Timestamp,
};
use alloc::vec::Vec;
use frame_support::{
	parameter_types,
	traits::{ConstU32, Equals},
};
use frame_system::EnsureRoot;
use pallet_xcm::EnsureXcm;

parameter_types! {
	/// The accounts that may drive the migration collectively. A constant, so the real set goes
	/// in with a runtime upgrade before a migration is scheduled. While it is empty only root
	/// and the appointed manager can act.
	pub MigrationMultisigMembers: Vec<AccountId> = Vec::new();
}

impl pallet_rc2_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type CtParaId = BrokerId;
	type TimeProvider = Timestamp;
	type CtOrigin = EnsureXcm<Equals<CoretimeLocation>>;
	type AdminOrigin = EnsureRoot<AccountId>;
	type MultisigMembers = MigrationMultisigMembers;
	type MultisigThreshold = ConstU32<3>;
	type MultisigMaxVotesPerRound = ConstU32<5>;
	// Polkadot and Kusama start a million rounds apart, more dispatches than either migration
	// will make; see `Config::MultisigStartRound`.
	type MultisigStartRound = ConstU32<1_000_000>;
}

#[cfg(test)]
mod tests {
	use crate::{Runtime, RuntimeCall};
	use codec::Encode;
	use pallet_ct_migrator::{Rc2MigratorCall, Rc2RuntimeCall};

	/// Ensure the pallet + call index aligns.
	#[test]
	fn the_coretime_chain_encodes_this_chains_calls_correctly() {
		assert_eq!(
			Rc2RuntimeCall::Rc2Migrator(Rc2MigratorCall::CtReady).encode(),
			RuntimeCall::Rc2Migrator(pallet_rc2_migrator::Call::<Runtime>::ct_ready {}).encode(),
		);
	}
}
