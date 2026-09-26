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
	xcm_config::{Broker, XcmRouter},
	AccountId, BrokerId, Runtime, RuntimeEvent, Timestamp,
};
use frame_support::traits::Equals;
use frame_system::EnsureRoot;
use pallet_xcm::EnsureXcm;

impl pallet_rc2_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type CtParaId = BrokerId;
	type TimeProvider = Timestamp;
	type CtOrigin = EnsureXcm<Equals<Broker>>;
	type AdminOrigin = EnsureRoot<AccountId>;
}

/// Contains all calls that are enabled once the migration has started.
///
/// Before the start every call is as `PostAhmFilter` has it. From the first block of the migration
/// a disabled call stays disabled, through `MigrationDone` and after: this chain holds nothing left
/// to act on. Everything that could change balances, reserves or holds while the data stages drain
/// them is disabled, since one taken while they run can leave value behind on this chain.
pub struct CallsEnabledDuringMigration;
impl frame_support::traits::Contains<crate::RuntimeCall> for CallsEnabledDuringMigration {
	fn contains(call: &crate::RuntimeCall) -> bool {
		use crate::RuntimeCall::*;
		const ON: bool = true;
		const OFF: bool = false;

		let enabled = match call {
			System(..) => ON, // Remarks, root calls and `set_code` if we need it for an emergency.
			Babe(..) => ON,   // For equivocation proof submissions; security relevant.
			Timestamp(..) => ON, // Only the `set` inherent.
			Indices(..) => OFF,
			Balances(..) => OFF,
			Staking(..) => OFF,
			Session(..) => ON, /* `set_keys` and `purge_keys` are closed by `PostAhmFilter`
			                     * already. */
			Grandpa(..) => ON, // For equivocation proof submissions; security relevant.
			Treasury(..) => OFF,
			ConvictionVoting(..) => OFF,
			Referenda(..) => OFF,
			FellowshipCollective(..) => ON, // Membership and votes; no deposits.
			FellowshipReferenda(..) => OFF, // Submission and decision deposits are reserves.
			Whitelist(..) => OFF,
			Parameters(..) => ON, // Root only.
			Claims(..) => OFF,
			Utility(..) => ON, // Batched calls go through this filter one by one.
			Society(..) => OFF,
			Vesting(..) => OFF,
			Scheduler(..) => OFF,
			// Using a proxy stays open; the proxied call goes through this filter. Every other
			// proxy call resizes a reserve.
			Proxy(
				pallet_proxy::Call::<Runtime>::proxy { .. } |
				pallet_proxy::Call::<Runtime>::proxy_announced { .. },
			) => ON,
			Proxy(..) => OFF,
			Multisig(..) => OFF,
			Preimage(..) => OFF,
			Bounties(..) => OFF,
			ChildBounties(..) => OFF,
			ElectionProviderMultiPhase(..) => OFF,
			VoterList(..) => OFF,
			NominationPools(..) => OFF,
			FastUnstake(..) => OFF,
			StakingAhClient(..) => ON, // Only permissioned calls, from Asset Hub.
			Configuration(..) => ON,   // Root only.
			ParasShared(..) => ON,     // Has no calls.
			ParaInclusion(..) => ON,   // Has no calls.
			ParaInherent(..) => ON,    // Only inherents.
			Paras(..) => ON,           // Root, and `include_pvf_check_statement` from validators.
			Initializer(..) => ON,     // Root only.
			Hrmp(..) => OFF,
			ParasDisputes(..) => ON, // Root only.
			ParasSlashing(..) => ON, // Security critical: dispute slashing reports.
			OnDemandAssignmentProvider(..) => OFF,
			Registrar(..) => OFF,
			Slots(..) => OFF,
			Auctions(..) => OFF,
			Crowdloan(..) => OFF,
			Coretime(..) => ON, // Only permissioned calls, from the Coretime chain.
			XcmPallet(..) => OFF,
			MessageQueue(..) => OFF, // `execute_overweight` would replay inbound XCM.
			AssetRate(..) => OFF,
			Beefy(..) => ON, // For equivocation proof submissions; security relevant.
			Rc2Migrator(..) => ON, // Only permissioned calls; drives the migration.
		};
		// Exhaustive match. Compiler ensures that we did not miss any.
		if !enabled {
			log::warn!("Call bounced by the filter during the migration: {call:?}");
		}
		enabled
	}
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
