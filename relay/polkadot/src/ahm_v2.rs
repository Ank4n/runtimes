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
	parachains_paras, parachains_slashing,
	xcm_config::{CoretimeLocation, XcmRouter},
	AccountId, BrokerId, Runtime, RuntimeCall, RuntimeEvent, Timestamp,
};
use frame_support::traits::{Contains, Equals, Everything};
use frame_system::EnsureRoot;
use pallet_xcm::EnsureXcm;

impl pallet_rc2_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type CtParaId = BrokerId;
	type TimeProvider = Timestamp;
	type CtOrigin = EnsureXcm<Equals<CoretimeLocation>>;
	type AdminOrigin = EnsureRoot<AccountId>;
	type PreMigrationCalls = Everything;
	type IntraMigrationCalls = CallsEnabledDuringMigration;
	type PostMigrationCalls = CallsEnabledAfterMigration;
}

/// Contains all calls that are enabled during the migration.
pub struct CallsEnabledDuringMigration;
impl Contains<RuntimeCall> for CallsEnabledDuringMigration {
	fn contains(call: &RuntimeCall) -> bool {
		let (during, _after) = call_allowed_status(call);
		if !during {
			log::warn!("Call bounced by the filter during the migration: {call:?}");
		}
		during
	}
}

/// Contains all calls that are enabled after the migration.
pub struct CallsEnabledAfterMigration;
impl Contains<RuntimeCall> for CallsEnabledAfterMigration {
	fn contains(call: &RuntimeCall) -> bool {
		let (_during, after) = call_allowed_status(call);
		if !after {
			log::warn!("Call bounced by the filter after the migration: {call:?}");
		}
		after
	}
}

/// Return whether a call is enabled during and after the migration.
///
/// During is from the start signal to the Coretime chain until the end of the cool-off. After is
/// from `MigrationDone` on.
///
/// Every call a signed account can make is disabled. Enabled are inherents, unsigned validator
/// reports, calls the Coretime chain and Asset Hub send over XCM, and the migrator itself.
///
/// Root skips this filter, so a call only Root can make is `OFF` here and still works.
pub fn call_allowed_status(call: &RuntimeCall) -> (bool, bool) {
	use RuntimeCall::*;
	const ON: bool = true;
	const OFF: bool = false;

	match call {
		// Applies a runtime upgrade governance already authorized.
		System(frame_system::Call::apply_authorized_upgrade { .. }) => (ON, ON),
		System(..) => (OFF, OFF),
		Scheduler(..) => (OFF, OFF),
		Preimage(..) => (OFF, OFF),
		Babe(pallet_babe::Call::report_equivocation_unsigned { .. }) => (ON, ON),
		Babe(..) => (OFF, OFF),
		// Only the `set` inherent.
		Timestamp(..) => (ON, ON),
		Indices(..) => (OFF, OFF),
		Balances(..) => (OFF, OFF),
		Staking(..) => (OFF, OFF),
		Session(..) => (OFF, OFF),
		Grandpa(pallet_grandpa::Call::report_equivocation_unsigned { .. }) => (ON, ON),
		Grandpa(..) => (OFF, OFF),
		Treasury(..) => (OFF, OFF),
		ConvictionVoting(..) => (OFF, OFF),
		Referenda(..) => (OFF, OFF),
		Whitelist(..) => (OFF, OFF),
		Claims(..) => (OFF, OFF),
		Vesting(..) => (OFF, OFF),
		Utility(..) => (OFF, OFF),
		Proxy(..) => (OFF, OFF),
		Multisig(..) => (OFF, OFF),
		Bounties(..) => (OFF, OFF),
		ChildBounties(..) => (OFF, OFF),
		ElectionProviderMultiPhase(..) => (OFF, OFF),
		VoterList(..) => (OFF, OFF),
		NominationPools(..) => (OFF, OFF),
		FastUnstake(..) => (OFF, OFF),
		// Asset Hub or the admin origin only. Era rotation should continue.
		StakingAhClient(..) => (ON, ON),
		// Asset Hub's StakingAdmin reaches it over XCM, not as Root.
		Parameters(..) => (ON, ON),
		// Root only.
		Configuration(..) => (OFF, OFF),
		ParasShared(..) => (OFF, OFF),
		ParaInclusion(..) => (OFF, OFF),
		// Only the `enter` inherent.
		ParaInherent(..) => (ON, ON),
		Paras(
			parachains_paras::Call::include_pvf_check_statement { .. } |
			parachains_paras::Call::apply_authorized_force_set_current_code { .. },
		) => (ON, ON),
		Paras(..) => (OFF, OFF),
		// Root only.
		Initializer(..) => (OFF, OFF),
		// TODO(ahm-v2): HRMP stays on this chain, with its deposits held on the Coretime chain.
		// Decide what opens after the migration.
		Hrmp(..) => (OFF, OFF),
		// Root only.
		ParasDisputes(..) => (OFF, OFF),
		ParasSlashing(parachains_slashing::Call::report_dispute_lost_unsigned { .. }) => (ON, ON),
		ParasSlashing(..) => (OFF, OFF),
		OnDemand(..) => (OFF, OFF),
		// TODO(ahm-v2): the registrar moves to the Coretime chain. Paras reach it through
		// `RegistrarRelay::relay_request`, which joins this list closed (before and) during
		// the migration and open after.
		Registrar(..) => (OFF, OFF),
		Slots(..) => (OFF, OFF),
		Auctions(..) => (OFF, OFF),
		Crowdloan(..) => (OFF, OFF),
		// The Coretime chain only.
		Coretime(..) => (ON, ON),
		XcmPallet(..) => (OFF, OFF),
		MessageQueue(..) => (OFF, OFF),
		AssetRate(..) => (OFF, OFF),
		Beefy(
			pallet_beefy::Call::report_double_voting_unsigned { .. } |
			pallet_beefy::Call::report_fork_voting_unsigned { .. } |
			pallet_beefy::Call::report_future_block_voting_unsigned { .. },
		) => (ON, ON),
		Beefy(..) => (OFF, OFF),
		// Checks its own origins; drives the migration.
		Rc2Migrator(..) => (ON, ON),
	}
}

#[cfg(test)]
mod tests {
	use crate::{parachains_paras, AccountId, Header, Runtime, RuntimeCall, RuntimeOrigin};
	use codec::Encode;
	use frame_support::traits::Contains;
	use pallet_ct_migrator::{Rc2MigratorCall, Rc2RuntimeCall};
	use pallet_rc2_migrator::{MigrationStageOf, RcMigrationStage};
	use sp_runtime::traits::{Dispatchable, Header as _};

	/// Ensure the pallet + call index aligns.
	#[test]
	fn the_coretime_chain_encodes_this_chains_calls_correctly() {
		assert_eq!(
			Rc2RuntimeCall::Rc2Migrator(Rc2MigratorCall::CtReady).encode(),
			RuntimeCall::Rc2Migrator(pallet_rc2_migrator::Call::<Runtime>::ct_ready {}).encode(),
		);
	}

	type Stage = MigrationStageOf<Runtime>;

	fn allowed_at(stage: &Stage, call: &RuntimeCall) -> bool {
		sp_io::TestExternalities::default().execute_with(|| {
			RcMigrationStage::<Runtime>::put(stage.clone());
			<Runtime as frame_system::Config>::BaseCallFilter::contains(call)
		})
	}

	fn stages_before_start() -> Vec<Stage> {
		vec![Stage::Pending, Stage::Scheduled { start: 0 }]
	}

	/// One stage from each part of the migration, and its end.
	fn stages_from_start() -> Vec<Stage> {
		vec![
			Stage::WaitingForCt,
			Stage::WarmUp { end_at: 0 },
			Stage::AccountsOngoing { last_key: None },
			Stage::CoolOff { end_at: 0 },
			Stage::MigrationDone,
		]
	}

	fn babe_equivocation_proof() -> Box<babe_primitives::EquivocationProof<Header>> {
		let header = Header::new(
			0,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
		);
		Box::new(babe_primitives::EquivocationProof {
			offender: sp_core::sr25519::Public::from_raw([0; 32]).into(),
			slot: 0.into(),
			first_header: header.clone(),
			second_header: header,
		})
	}

	fn key_owner_proof() -> sp_session::MembershipProof {
		sp_session::MembershipProof { session: 0, trie_nodes: vec![], validator_count: 0 }
	}

	#[test]
	fn signed_calls_are_refused_once_the_migration_starts() {
		let alice = AccountId::new([1; 32]); // any signed account
		let remark = RuntimeCall::System(frame_system::Call::remark { remark: vec![1] });
		let calls = [
			RuntimeCall::Balances(pallet_balances::Call::transfer_keep_alive {
				dest: alice.clone().into(),
				value: 1,
			}),
			remark.clone(),
			RuntimeCall::Utility(pallet_utility::Call::batch { calls: vec![remark.clone()] }),
			RuntimeCall::Proxy(pallet_proxy::Call::proxy {
				real: alice.into(),
				force_proxy_type: None,
				call: Box::new(remark),
			}),
			RuntimeCall::Babe(pallet_babe::Call::report_equivocation {
				equivocation_proof: babe_equivocation_proof(),
				key_owner_proof: key_owner_proof(),
			}),
			RuntimeCall::Paras(parachains_paras::Call::remove_upgrade_cooldown {
				para: 2000.into(),
			}),
		];

		for call in calls {
			// GIVEN the migration has not started. THEN the call passes.
			for stage in stages_before_start() {
				assert!(allowed_at(&stage, &call), "{call:?} refused at {stage:?}");
			}
			// GIVEN the migration has started. THEN the call is refused, also after it is done.
			for stage in stages_from_start() {
				assert!(!allowed_at(&stage, &call), "{call:?} allowed at {stage:?}");
			}
		}
	}

	#[test]
	fn consensus_and_permissioned_calls_pass_at_every_stage() {
		let calls = [
			RuntimeCall::Timestamp(pallet_timestamp::Call::set { now: 0 }),
			RuntimeCall::System(frame_system::Call::apply_authorized_upgrade { code: vec![] }),
			RuntimeCall::Babe(pallet_babe::Call::report_equivocation_unsigned {
				equivocation_proof: babe_equivocation_proof(),
				key_owner_proof: key_owner_proof(),
			}),
			RuntimeCall::Paras(parachains_paras::Call::apply_authorized_force_set_current_code {
				para: 2000.into(),
				new_code: vec![].into(),
			}),
			RuntimeCall::Coretime(runtime_parachains::coretime::Call::request_core_count {
				count: 1,
			}),
			RuntimeCall::StakingAhClient(pallet_staking_async_ah_client::Call::set_mode {
				mode: pallet_staking_async_ah_client::OperatingMode::Active,
			}),
			RuntimeCall::Rc2Migrator(pallet_rc2_migrator::Call::ct_ready {}),
		];

		for call in calls {
			for stage in stages_before_start().into_iter().chain(stages_from_start()) {
				assert!(allowed_at(&stage, &call), "{call:?} refused at {stage:?}");
			}
		}
	}

	#[test]
	fn root_skips_the_filter() {
		let alice = AccountId::new([1; 32]); // any signed account
		let remark = RuntimeCall::System(frame_system::Call::remark { remark: vec![1] });

		for stage in stages_from_start() {
			sp_io::TestExternalities::default().execute_with(|| {
				// GIVEN the migration at `stage`.
				RcMigrationStage::<Runtime>::put(stage.clone());

				// THEN a signed account's remark is filtered, and Root's is not.
				assert_eq!(
					remark
						.clone()
						.dispatch(RuntimeOrigin::signed(alice.clone()))
						.map_err(|e| e.error),
					Err(frame_system::Error::<Runtime>::CallFiltered.into()),
					"at {stage:?}"
				);
				assert!(remark.clone().dispatch(RuntimeOrigin::root()).is_ok(), "at {stage:?}");
			});
		}
	}
}
