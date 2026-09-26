// Copyright (C) Parity Technologies (UK) Ltd.
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
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! What the AHM v2 migration closes on this chain, and what it must leave open.

#![cfg(all(feature = "ahm-v2", not(feature = "on-chain-release-build")))]

use frame_support::traits::Contains;
use kusama_runtime_constants::{
	currency::UNITS,
	proxy::ProxyType,
	system_parachain::{ASSET_HUB_ID, BRIDGE_HUB_ID, BROKER_ID},
};
use pallet_rc2_migrator::{MigrationStageOf, Paused, RcMigrationStage};
use polkadot_primitives::{AccountId, HeadData, HrmpChannelId};
use polkadot_runtime_common::{crowdloan, paras_registrar};
use runtime_parachains::{
	hrmp,
	inclusion::{AggregateMessageOrigin, UmpQueueId},
	on_demand,
};
use sp_io::TestExternalities;
use staging_kusama_runtime::{
	xcm_config::XcmConfig, PostAhmFilter, Runtime, RuntimeCall, TransparentProxyType,
};
use xcm::latest::prelude::*;
use xcm_executor::XcmExecutor;

type Stage = MigrationStageOf<Runtime>;

const ALICE: AccountId = AccountId::new([1u8; 32]);

/// The filter reads the migration stage, so it needs storage.
fn allowed_at(stage: &Stage, call: &RuntimeCall) -> bool {
	TestExternalities::default().execute_with(|| {
		RcMigrationStage::<Runtime>::put(stage.clone());
		PostAhmFilter::contains(call)
	})
}

/// Every stage in which the relay chain still serves its users, and every stage in which it must
/// not. `MigrationDone` sits with the latter: what the migration closes does not reopen.
fn open_stages() -> [Stage; 2] {
	[Stage::Pending, Stage::Scheduled { start: 1_000 }]
}
fn closed_stages() -> [Stage; 5] {
	[
		Stage::WaitingForCt,
		Stage::WarmUp { end_at: 10 },
		Stage::AccountsOngoing { last_key: None },
		Stage::CoolOff { end_at: 10 },
		Stage::MigrationDone,
	]
}

fn assert_closes_at_migration_start(call: RuntimeCall) {
	for stage in open_stages() {
		assert!(allowed_at(&stage, &call), "{call:?} must stay open at {stage:?}");
	}
	for stage in closed_stages() {
		assert!(!allowed_at(&stage, &call), "{call:?} must be closed at {stage:?}");
	}
}

fn assert_never_closes(call: RuntimeCall) {
	for stage in open_stages().into_iter().chain(closed_stages()) {
		assert!(allowed_at(&stage, &call), "{call:?} must stay open at {stage:?}");
	}
}

/// The calls a signed origin could use to move value or resize a reserve while the accounts stage
/// is draining them.
///
/// One per pallet the filter names: the arms match on the pallet, so a single call witnesses each.
#[test]
fn value_movers_close_when_the_migration_starts() {
	for call in [
		RuntimeCall::Balances(pallet_balances::Call::transfer_allow_death {
			dest: ALICE.into(),
			value: UNITS,
		}),
		RuntimeCall::XcmPallet(pallet_xcm::Call::transfer_assets {
			dest: Box::new(Parachain(ASSET_HUB_ID).into_location().into_versioned()),
			beneficiary: Box::new(
				Location::new(0, [AccountId32 { network: None, id: ALICE.into() }])
					.into_versioned(),
			),
			assets: Box::new(Assets::from(vec![(Here, UNITS).into()]).into()),
			fee_asset_item: 0,
			weight_limit: Unlimited,
		}),
		RuntimeCall::Multisig(pallet_multisig::Call::approve_as_multi {
			threshold: 2,
			other_signatories: vec![ALICE],
			maybe_timepoint: None,
			call_hash: [0u8; 32],
			max_weight: Weight::zero(),
		}),
		RuntimeCall::Preimage(pallet_preimage::Call::note_preimage { bytes: vec![1, 2, 3] }),
		RuntimeCall::OnDemandAssignmentProvider(on_demand::Call::place_order_allow_death {
			max_amount: UNITS,
			para_id: 2000.into(),
		}),
		RuntimeCall::Crowdloan(crowdloan::Call::withdraw { who: ALICE, index: 0.into() }),
	] {
		assert_closes_at_migration_start(call);
	}
}

/// Using a proxy is not a value movement; changing the proxy map or an announcement is, because
/// both resize a reserve.
#[test]
fn proxies_keep_working_but_stop_changing() {
	for call in [
		RuntimeCall::Proxy(pallet_proxy::Call::add_proxy {
			delegate: ALICE.into(),
			proxy_type: TransparentProxyType(ProxyType::Any),
			delay: 0,
		}),
		RuntimeCall::Proxy(pallet_proxy::Call::announce {
			real: ALICE.into(),
			call_hash: Default::default(),
		}),
		RuntimeCall::Proxy(pallet_proxy::Call::poke_deposit {}),
	] {
		assert_closes_at_migration_start(call);
	}

	assert_never_closes(RuntimeCall::Proxy(pallet_proxy::Call::proxy {
		real: ALICE.into(),
		force_proxy_type: None,
		call: Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![] })),
	}));
}

/// The parachain control plane, which closes at the migration's first block rather than at the
/// runtime upgrade.
#[test]
fn the_parachain_control_plane_closes_when_the_migration_starts() {
	for call in [
		RuntimeCall::Registrar(paras_registrar::Call::reserve {}),
		RuntimeCall::Registrar(paras_registrar::Call::deregister { id: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::add_lock { para: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::set_current_head {
			para: 2000.into(),
			new_head: HeadData(vec![1, 2, 3]),
		}),
		RuntimeCall::Hrmp(hrmp::Call::hrmp_init_open_channel {
			recipient: 2001.into(),
			proposed_max_capacity: 8,
			proposed_max_message_size: 1024,
		}),
		RuntimeCall::Hrmp(hrmp::Call::hrmp_close_channel {
			channel_id: HrmpChannelId { sender: 2000.into(), recipient: 2001.into() },
		}),
		RuntimeCall::Hrmp(hrmp::Call::establish_channel_with_system {
			target_system_chain: ASSET_HUB_ID.into(),
		}),
	] {
		assert_closes_at_migration_start(call);
	}
}

/// Pausing stops the machine, not the lockdown: the stage is untouched and the filter reads only
/// the stage.
#[test]
fn a_paused_migration_keeps_the_filter_closed() {
	let call = RuntimeCall::Balances(pallet_balances::Call::transfer_allow_death {
		dest: ALICE.into(),
		value: UNITS,
	});
	TestExternalities::default().execute_with(|| {
		RcMigrationStage::<Runtime>::put(Stage::AccountsOngoing { last_key: None });
		Paused::<Runtime>::put(true);
		assert!(!PostAhmFilter::contains(&call), "{call:?} must stay closed while paused");
	});
}

/// Replaying an inbound message would run XCM from a chain the barrier now refuses.
#[test]
fn replaying_inbound_messages_closes_when_the_migration_starts() {
	assert_closes_at_migration_start(RuntimeCall::MessageQueue(
		pallet_message_queue::Call::execute_overweight {
			message_origin: AggregateMessageOrigin::Ump(UmpQueueId::Para(2000.into())),
			page: 0,
			index: 0,
			weight_limit: Weight::zero(),
		},
	));
}

/// Calls the migration leaves open are unaffected at every stage.
#[test]
fn calls_the_migration_leaves_open_are_untouched() {
	let remark = RuntimeCall::System(frame_system::Call::remark { remark: vec![1, 2, 3] });
	assert_never_closes(remark.clone());
	assert_never_closes(RuntimeCall::Utility(pallet_utility::Call::batch { calls: vec![remark] }));
}

/// The executor's teleport trust, which a call filter cannot cover: an inbound
/// `ReceiveTeleportedAsset` dispatches nothing on this chain.
fn teleport_from_asset_hub(stage: &Stage) -> Outcome {
	TestExternalities::default().execute_with(|| {
		RcMigrationStage::<Runtime>::put(stage.clone());
		let message = Xcm::<RuntimeCall>(vec![
			ReceiveTeleportedAsset(Assets::from(vec![(Here, 10 * UNITS).into()])),
			DepositAsset {
				assets: AllCounted(1).into(),
				beneficiary: Location::new(0, [AccountId32 { network: None, id: ALICE.into() }]),
			},
		]);
		let weight = Weight::from_parts(10_000_000_000, 1_000_000);
		XcmExecutor::<XcmConfig>::prepare_and_execute(
			Parachain(ASSET_HUB_ID),
			message,
			&mut [0u8; 32],
			weight,
			weight,
		)
	})
}

#[test]
fn teleports_from_a_system_chain_land_until_the_migration_starts() {
	for stage in open_stages() {
		assert_eq!(
			teleport_from_asset_hub(&stage).ensure_complete(),
			Ok(()),
			"teleports must still land at {stage:?}"
		);
	}
}

#[test]
fn teleports_are_refused_once_the_migration_starts() {
	for stage in closed_stages() {
		let error = teleport_from_asset_hub(&stage)
			.ensure_complete()
			.expect_err("teleport must be refused");
		assert_eq!(
			error.error,
			XcmError::UntrustedTeleportLocation,
			"teleport must be refused as untrusted at {stage:?}"
		);
	}
}

/// Execute `message` as if it arrived from `origin`, returning the error it stopped with.
fn execute_from(
	stage: &Stage,
	origin: Location,
	message: Xcm<RuntimeCall>,
) -> Result<(), XcmError> {
	TestExternalities::default().execute_with(|| {
		RcMigrationStage::<Runtime>::put(stage.clone());
		let weight = Weight::from_parts(10_000_000_000, 1_000_000);
		XcmExecutor::<XcmConfig>::prepare_and_execute(
			origin,
			message,
			&mut [0u8; 32],
			weight,
			weight,
		)
		.ensure_complete()
		.map_err(|e| e.error)
	})
}

/// A message a system chain may send for free.
fn unpaid() -> Xcm<RuntimeCall> {
	Xcm(vec![UnpaidExecution { weight_limit: Unlimited, check_origin: None }, ClearOrigin])
}

/// A message any chain may send if it pays: it would move its sovereign's balance to Alice.
fn paid() -> Xcm<RuntimeCall> {
	Xcm(vec![
		WithdrawAsset((Here, UNITS).into()),
		BuyExecution { fees: (Here, UNITS).into(), weight_limit: Unlimited },
		DepositAsset {
			assets: AllCounted(1).into(),
			beneficiary: Location::new(0, [AccountId32 { network: None, id: ALICE.into() }]),
		},
	])
}

/// The barrier's gate, which a call filter cannot cover: a message from another chain runs XCM
/// instructions here without dispatching anything.
#[test]
fn only_coretime_and_asset_hub_reach_this_chain_once_the_migration_starts() {
	let para = || Location::new(0, [Parachain(2000)]);
	let system = |id| Location::new(0, [Parachain(id)]);

	for stage in open_stages() {
		// The barrier admits it; it fails later only because the sovereign holds nothing.
		assert_ne!(execute_from(&stage, para(), paid()), Err(XcmError::Barrier), "at {stage:?}");
		assert_eq!(execute_from(&stage, system(BRIDGE_HUB_ID), unpaid()), Ok(()), "at {stage:?}");
	}
	for stage in closed_stages() {
		assert_eq!(execute_from(&stage, para(), paid()), Err(XcmError::Barrier), "at {stage:?}");
		assert_eq!(
			execute_from(&stage, system(BRIDGE_HUB_ID), unpaid()),
			Err(XcmError::Barrier),
			"at {stage:?}"
		);
		assert_eq!(execute_from(&stage, system(BROKER_ID), unpaid()), Ok(()), "at {stage:?}");
		assert_eq!(execute_from(&stage, system(ASSET_HUB_ID), unpaid()), Ok(()), "at {stage:?}");
	}
}
