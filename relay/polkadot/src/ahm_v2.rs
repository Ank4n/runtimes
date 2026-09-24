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
	AccountId, BrokerId, Runtime, RuntimeCall, RuntimeEvent, Timestamp,
};
use alloc::vec::Vec;
use frame_support::{parameter_types, traits::Equals};
use frame_system::EnsureRoot;
use pallet_xcm::EnsureXcm;

parameter_types! {
	/// The accounts that may drive the migration collectively. Governance seeds the real set
	/// before a migration is scheduled; empty means only root and the appointed manager can act.
	pub MigrationMultisigMembers: Vec<AccountId> = Vec::new();
	/// Votes needed from distinct members.
	pub const MigrationMultisigThreshold: u32 = 3;
	/// Votes one member may cast per round.
	pub const MigrationMultisigMaxVotesPerRound: u32 = 5;
	/// A vote is signed over (who, call, round) and nothing else, so two networks sitting at the
	/// same round would accept each other's signatures. This is what keeps them apart.
	pub const MigrationMultisigStartRound: u32 = 100;
}

impl pallet_rc2_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type CtParaId = BrokerId;
	type TimeProvider = Timestamp;
	type CtOrigin = EnsureXcm<Equals<CoretimeLocation>>;
	type AdminOrigin = EnsureRoot<AccountId>;
	type RuntimeCall = RuntimeCall;
	type MultisigMembers = MigrationMultisigMembers;
	type MultisigThreshold = MigrationMultisigThreshold;
	type MultisigMaxVotesPerRound = MigrationMultisigMaxVotesPerRound;
	type MultisigStartRound = MigrationMultisigStartRound;
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
