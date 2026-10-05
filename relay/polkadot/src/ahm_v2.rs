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
use frame_support::{
	traits::{Contains, Equals, Everything, ProcessMessageError},
	weights::Weight,
};
use frame_system::EnsureRoot;
use pallet_rc2_migrator::RcMigrationStage;
use pallet_xcm::EnsureXcm;
use polkadot_runtime_constants::system_parachain::{ASSET_HUB_ID, BROKER_ID};
use xcm::latest::{Instruction, Junction::Parachain, Location};
use xcm_executor::traits::{DenyExecution, Properties};

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

/// Refuses every inbound message once the migration starts, except from this chain, the Coretime
/// chain and Asset Hub.
pub struct DenyOnceMigrationStarts;
impl DenyExecution for DenyOnceMigrationStarts {
	fn deny_execution<RuntimeCall>(
		origin: &Location,
		_instructions: &mut [Instruction<RuntimeCall>],
		_max_weight: Weight,
		_properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		let stage = RcMigrationStage::<Runtime>::get();
		if !stage.has_started() {
			Ok(())
		} else if stage.is_ongoing() {
			only_from_self_coretime_or_asset_hub(origin)
		} else {
			// TODO(ahm-v2): paras reach `HrmpRelay` and `RegistrarRelay` over XCM after the
			// migration; let that envelope through here.
			only_from_self_coretime_or_asset_hub(origin)
		}
	}
}

/// CT assigns cores and answers the migrator. AH governance dispatches here as Root / admin body,
/// and its staking sends the validator set and session keys.
fn only_from_self_coretime_or_asset_hub(origin: &Location) -> Result<(), ProcessMessageError> {
	match origin.unpack() {
		(0, []) => Ok(()),
		(0, [Parachain(id)]) if *id == BROKER_ID || *id == ASSET_HUB_ID => Ok(()),
		_ => Err(ProcessMessageError::Unsupported),
	}
}

/// Contains all calls that are enabled during the migration.
pub struct CallsEnabledDuringMigration;
impl Contains<RuntimeCall> for CallsEnabledDuringMigration {
	fn contains(call: &RuntimeCall) -> bool {
		call_allowed_status(call).0
	}
}

/// Contains all calls that are enabled after the migration.
pub struct CallsEnabledAfterMigration;
impl Contains<RuntimeCall> for CallsEnabledAfterMigration {
	fn contains(call: &RuntimeCall) -> bool {
		call_allowed_status(call).1
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
		// Enabled during and after the migration.
		// Applies a runtime upgrade governance already authorized.
		System(frame_system::Call::apply_authorized_upgrade { .. }) => (ON, ON),
		Babe(pallet_babe::Call::report_equivocation_unsigned { .. }) => (ON, ON),
		// Only the `set` inherent.
		Timestamp(..) => (ON, ON),
		Grandpa(pallet_grandpa::Call::report_equivocation_unsigned { .. }) => (ON, ON),
		// Asset Hub or the admin origin only. Era rotation should continue.
		StakingAhClient(..) => (ON, ON),
		// Asset Hub's StakingAdmin reaches it over XCM, not as Root.
		Parameters(..) => (ON, ON),
		// Only the `enter` inherent.
		ParaInherent(..) => (ON, ON),
		Paras(
			parachains_paras::Call::include_pvf_check_statement { .. } |
			parachains_paras::Call::apply_authorized_force_set_current_code { .. },
		) => (ON, ON),
		ParasSlashing(parachains_slashing::Call::report_dispute_lost_unsigned { .. }) => (ON, ON),
		// The Coretime chain only.
		Coretime(..) => (ON, ON),
		Beefy(
			pallet_beefy::Call::report_double_voting_unsigned { .. } |
			pallet_beefy::Call::report_fork_voting_unsigned { .. } |
			pallet_beefy::Call::report_future_block_voting_unsigned { .. },
		) => (ON, ON),
		// Checks its own origins; drives the migration.
		Rc2Migrator(..) => (ON, ON),

		// Disabled during and after the migration.
		System(..) => (OFF, OFF),
		Scheduler(..) => (OFF, OFF),
		Preimage(..) => (OFF, OFF),
		Babe(..) => (OFF, OFF),
		Indices(..) => (OFF, OFF),
		Balances(..) => (OFF, OFF),
		Staking(..) => (OFF, OFF),
		Session(..) => (OFF, OFF),
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
		ParasShared(..) => (OFF, OFF),
		ParaInclusion(..) => (OFF, OFF),
		Paras(..) => (OFF, OFF),
		// Root only.
		Configuration(..) | Initializer(..) | ParasDisputes(..) => (OFF, OFF),
		// TODO(ahm-v2): HRMP stays on this chain, with its deposits held on the Coretime chain.
		// Decide what opens after the migration.
		Hrmp(..) => (OFF, OFF),
		ParasSlashing(..) => (OFF, OFF),
		OnDemand(..) => (OFF, OFF),
		// TODO(ahm-v2): the registrar moves to the Coretime chain. Paras reach it through
		// `RegistrarRelay::relay_request`, which joins this list closed (before and) during
		// the migration and open after.
		Registrar(..) => (OFF, OFF),
		Slots(..) => (OFF, OFF),
		Auctions(..) => (OFF, OFF),
		Crowdloan(..) => (OFF, OFF),
		XcmPallet(..) => (OFF, OFF),
		MessageQueue(..) => (OFF, OFF),
		AssetRate(..) => (OFF, OFF),
		Beefy(..) => (OFF, OFF),
	}
}

#[cfg(test)]
mod tests {
	use crate::{
		parachains_paras, xcm_config::SovereignAccountOf, AccountId, Balances, Header,
		PostAhmFilter, Runtime, RuntimeCall, RuntimeOrigin,
	};
	use codec::Encode;
	use frame_support::traits::{fungible::Mutate, Contains};
	use pallet_ct_migrator::{Rc2MigratorCall, Rc2RuntimeCall};
	use pallet_rc2_migrator::{test_utils::every_call, MigrationStageOf, RcMigrationStage};
	use polkadot_runtime_constants::{
		currency::UNITS,
		system_parachain::{ASSET_HUB_ID, BRIDGE_HUB_ID, BROKER_ID},
	};
	use sp_runtime::traits::{Dispatchable, Header as _};
	use xcm::latest::prelude::*;
	use xcm_executor::{traits::ConvertLocation, XcmExecutor};

	/// Ensure the pallet + call index aligns.
	#[test]
	fn the_coretime_chain_encodes_this_chains_calls_correctly() {
		assert_eq!(
			Rc2RuntimeCall::Rc2Migrator(Rc2MigratorCall::CtReady).encode(),
			RuntimeCall::Rc2Migrator(pallet_rc2_migrator::Call::<Runtime>::ct_ready {}).encode(),
		);
	}

	type Stage = MigrationStageOf<Runtime>;

	/// Any account.
	const ALICE: AccountId = AccountId::new([1; 32]);

	/// Run `f` with the migration at `stage`.
	fn at<R>(stage: &Stage, f: impl FnOnce() -> R) -> R {
		sp_io::TestExternalities::default().execute_with(|| {
			RcMigrationStage::<Runtime>::put(stage);
			f()
		})
	}

	fn allowed_at(stage: &Stage, call: &RuntimeCall) -> bool {
		at(stage, || <Runtime as frame_system::Config>::BaseCallFilter::contains(call))
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
		let remark = RuntimeCall::System(frame_system::Call::remark { remark: vec![1] });
		let calls = [
			RuntimeCall::Balances(pallet_balances::Call::transfer_keep_alive {
				dest: ALICE.into(),
				value: 1,
			}),
			remark.clone(),
			RuntimeCall::Utility(pallet_utility::Call::batch { calls: vec![remark.clone()] }),
			RuntimeCall::Proxy(pallet_proxy::Call::proxy {
				real: ALICE.into(),
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

	/// Every call the lockdown leaves enabled: inherents, unsigned validator reports, applying code
	/// governance already authorized, calls only the Coretime chain or Asset Hub can make, and the
	/// migrator's own.
	const ENABLED: &[(&str, &[&str])] = &[
		("System", &["apply_authorized_upgrade"]),
		("Babe", &["report_equivocation_unsigned"]),
		("Timestamp", &["set"]),
		("Grandpa", &["report_equivocation_unsigned"]),
		(
			"StakingAhClient",
			&[
				"validator_set",
				"set_mode",
				"force_on_migration_end",
				"set_keys_from_ah",
				"purge_keys_from_ah",
			],
		),
		("Parameters", &["set_parameter"]),
		("ParaInherent", &["enter"]),
		("Paras", &["include_pvf_check_statement", "apply_authorized_force_set_current_code"]),
		("ParasSlashing", &["report_dispute_lost_unsigned"]),
		(
			"Coretime",
			// CLAUDE: similar to other comment, we don't need request revenue and credit_account, right?
			// I guess no harm in keeping, but in any case we should leave a comment.
			&["request_core_count", "request_revenue_at", "credit_account", "assign_core"],
		),
		(
			"Beefy",
			&[
				"report_double_voting_unsigned",
				"report_fork_voting_unsigned",
				"report_future_block_voting_unsigned",
			],
		),
		(
			"Rc2Migrator",
			&[
				"schedule_migration",
				"cancel_migration",
				"ct_ready",
				"pause_migration",
				"resume_migration",
				"force_set_stage",
				"set_manager",
			],
		),
	];

	#[test]
	fn every_call_but_the_listed_ones_is_refused_once_the_migration_starts() {
		let calls = every_call::<RuntimeCall>();
		let listed = |pallet: &str, name: &str| {
			ENABLED.iter().any(|(p, names)| *p == pallet && names.contains(&name))
		};

		// Every listed call is a call of this runtime.
		for (pallet, names) in ENABLED {
			for name in *names {
				assert!(
					calls.iter().any(|(p, n, _)| p == pallet && n == name),
					"{pallet}::{name} is not a call of this runtime"
				);
			}
		}

		// GIVEN the migration has not started. THEN the lockdown changes nothing.
		// GIVEN the migration has started. THEN only a listed call passes, also after it is done.
		for stage in stages_before_start().into_iter().chain(stages_from_start()) {
			at(&stage, || {
				for (pallet, name, call) in &calls {
					let expected = if stage.has_started() {
						listed(pallet, name)
					} else {
						PostAhmFilter::contains(call)
					};
					assert_eq!(
						<Runtime as frame_system::Config>::BaseCallFilter::contains(call),
						expected,
						"{pallet}::{name} at {stage:?}"
					);
				}
			});
		}
	}

	#[test]
	fn root_skips_the_filter() {
		let remark = RuntimeCall::System(frame_system::Call::remark { remark: vec![1] });

		for stage in stages_from_start() {
			at(&stage, || {
				// THEN a signed account's remark is filtered, and Root's is not.
				assert_eq!(
					remark.clone().dispatch(RuntimeOrigin::signed(ALICE)).map_err(|e| e.error),
					Err(frame_system::Error::<Runtime>::CallFiltered.into()),
					"at {stage:?}"
				);
				assert!(remark.clone().dispatch(RuntimeOrigin::root()).is_ok(), "at {stage:?}");
			});
		}
	}

	fn teleport_from_asset_hub(stage: &Stage) -> Outcome {
		at(stage, || {
			let message = Xcm::<RuntimeCall>(vec![
				ReceiveTeleportedAsset((Here, 10 * UNITS).into()),
				DepositAsset {
					assets: AllCounted(1).into(),
					beneficiary: AccountId32 { network: None, id: ALICE.into() }.into(),
				},
			]);
			// No weight limit, and the weight counts as paid so the barrier admits a message that
			// does not buy execution.
			XcmExecutor::<crate::xcm_config::XcmConfig>::prepare_and_execute(
				Parachain(ASSET_HUB_ID),
				message,
				&mut [0u8; 32],
				Weight::MAX,
				Weight::MAX,
			)
		})
	}

	#[test]
	fn inbound_teleports_are_refused_once_the_migration_starts() {
		// GIVEN the migration has not started. THEN a teleport from Asset Hub lands.
		for stage in stages_before_start() {
			assert_eq!(teleport_from_asset_hub(&stage).ensure_complete(), Ok(()), "at {stage:?}");
		}

		// GIVEN the migration has started. THEN the same teleport is refused as untrusted, also
		// after it is done.
		for stage in stages_from_start() {
			assert_eq!(
				teleport_from_asset_hub(&stage).ensure_complete().map_err(|e| e.error),
				Err(XcmError::UntrustedTeleportLocation),
				"at {stage:?}"
			);
		}
	}

	/// Execute `message` from `origin`, with `origin`'s account on this chain funded so a paid
	/// message can complete.
	fn execute_from(
		stage: &Stage,
		origin: Location,
		message: Xcm<RuntimeCall>,
	) -> Result<(), XcmError> {
		at(stage, || {
			let account = SovereignAccountOf::convert_location(&origin).expect("origin converts");
			<Balances as Mutate<AccountId>>::mint_into(&account, 100 * UNITS).unwrap();
			XcmExecutor::<crate::xcm_config::XcmConfig>::prepare_and_execute(
				origin,
				message,
				&mut [0u8; 32],
				Weight::MAX,
				Weight::zero(),
			)
			.ensure_complete()
			.map_err(|e| e.error)
		})
	}

	fn unpaid() -> Xcm<RuntimeCall> {
		Xcm(vec![UnpaidExecution { weight_limit: Unlimited, check_origin: None }, ClearOrigin])
	}

	fn paid() -> Xcm<RuntimeCall> {
		Xcm(vec![
			WithdrawAsset((Here, 10 * UNITS).into()),
			BuyExecution { fees: (Here, UNITS).into(), weight_limit: Unlimited },
			DepositAsset {
				assets: AllCounted(1).into(),
				beneficiary: AccountId32 { network: None, id: ALICE.into() }.into(),
			},
		])
	}

	#[test]
	fn only_coretime_and_asset_hub_reach_this_chain_once_the_migration_starts() {
		let para = || Location::new(0, [Parachain(2000)]);
		let system = |id| Location::new(0, [Parachain(id)]);

		// GIVEN the migration has not started. THEN a para and any system chain get through.
		for stage in stages_before_start() {
			assert_eq!(execute_from(&stage, para(), paid()), Ok(()), "at {stage:?}");
			assert_eq!(execute_from(&stage, system(BRIDGE_HUB_ID), paid()), Ok(()), "at {stage:?}");
			assert_eq!(
				execute_from(&stage, system(BRIDGE_HUB_ID), unpaid()),
				Ok(()),
				"at {stage:?}"
			);
		}

		// GIVEN the migration has started. THEN a para and every other system chain are refused,
		// paid or unpaid, also after it is done. The Coretime chain and Asset Hub get through.
		for stage in stages_from_start() {
			for message in [paid(), unpaid()] {
				for origin in [para(), system(BRIDGE_HUB_ID)] {
					assert_eq!(
						execute_from(&stage, origin.clone(), message.clone()),
						Err(XcmError::Barrier),
						"{origin:?} at {stage:?}"
					);
				}
				for origin in [system(BROKER_ID), system(ASSET_HUB_ID)] {
					assert_eq!(
						execute_from(&stage, origin.clone(), message.clone()),
						Ok(()),
						"{origin:?} at {stage:?}"
					);
				}
			}
		}
	}
}
