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
use pallet_rc2_migrator::{MigrationStageOf, RcMigrationStage};
use polkadot_primitives::{AccountId, HeadData, HrmpChannelId};
use polkadot_runtime::{xcm_config::XcmConfig, PostAhmFilter, Runtime, RuntimeCall};
use polkadot_runtime_common::{crowdloan, paras_registrar};
use polkadot_runtime_constants::{currency::UNITS, system_parachain::ASSET_HUB_ID};
use runtime_parachains::hrmp;
use sp_runtime::BuildStorage;
use xcm::latest::prelude::*;
use xcm_executor::XcmExecutor;

type Stage = MigrationStageOf<Runtime>;

const ALICE: AccountId = AccountId::new([1u8; 32]);

fn new_test_ext() -> sp_io::TestExternalities {
	frame_system::GenesisConfig::<Runtime>::default()
		.build_storage()
		.unwrap()
		.into()
}

/// The filter reads the migration stage, so it needs storage.
fn allowed_at(stage: &Stage, call: &RuntimeCall) -> bool {
	new_test_ext().execute_with(|| {
		RcMigrationStage::<Runtime>::put(stage.clone());
		PostAhmFilter::contains(call)
	})
}

/// Every stage in which the relay chain still serves its users, and every stage in which it must
/// not. `MigrationDone` sits with the latter: what the migration closes does not reopen.
fn open_stages() -> [Stage; 2] {
	[Stage::Pending, Stage::Scheduled { start: 1_000 }]
}
// TODO(ahm-v2): with the stage machine, `Paused` becomes a flag beside the stage rather than a
// variant, and `AccountsOngoing` exists to stand in for `RegistrarOngoing` here.
fn closed_stages() -> [Stage; 5] {
	[
		Stage::WaitingForCt,
		Stage::RegistrarOngoing { last_key: None },
		Stage::Paused,
		Stage::CoolOff { end_at: 10 },
		Stage::MigrationDone,
	]
}

fn every_stage() -> impl Iterator<Item = Stage> {
	open_stages().into_iter().chain(closed_stages())
}

fn assert_closes_at_migration_start(call: RuntimeCall) {
	for stage in open_stages() {
		assert!(allowed_at(&stage, &call), "{call:?} must stay open at {stage:?}");
	}
	for stage in closed_stages() {
		assert!(!allowed_at(&stage, &call), "{call:?} must be closed at {stage:?}");
	}
}

/// The calls a signed origin could use to move value or resize a reserve while the accounts stage
/// is draining them.
///
/// One per pallet the filter names: the arms match on the pallet, so a single call witnesses each.
#[test]
fn value_movers_close_when_the_migration_starts() {
	for call in [
		RuntimeCall::Balances(pallet_balances::Call::<Runtime>::transfer_allow_death {
			dest: ALICE.into(),
			value: UNITS,
		}),
		RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::transfer_assets {
			dest: Box::new(Parachain(ASSET_HUB_ID).into_location().into_versioned()),
			beneficiary: Box::new(
				Location::new(0, [AccountId32 { network: None, id: ALICE.into() }])
					.into_versioned(),
			),
			assets: Box::new(Assets::from(vec![(Here, UNITS).into()]).into()),
			fee_asset_item: 0,
			weight_limit: Unlimited,
		}),
		RuntimeCall::Multisig(pallet_multisig::Call::<Runtime>::approve_as_multi {
			threshold: 2,
			other_signatories: vec![ALICE],
			maybe_timepoint: None,
			call_hash: [0u8; 32],
			max_weight: Weight::zero(),
		}),
		RuntimeCall::Preimage(pallet_preimage::Call::<Runtime>::note_preimage {
			bytes: vec![1, 2, 3],
		}),
		RuntimeCall::OnDemand(
			runtime_parachains::on_demand::Call::<Runtime>::place_order_allow_death {
				max_amount: UNITS,
				para_id: 2000.into(),
			},
		),
		RuntimeCall::Crowdloan(crowdloan::Call::<Runtime>::withdraw {
			who: ALICE,
			index: 0.into(),
		}),
	] {
		assert_closes_at_migration_start(call);
	}
}

/// Using a proxy is not a value movement; changing the proxy map or an announcement is, because
/// both resize a reserve.
#[test]
fn proxies_keep_working_but_stop_changing() {
	let add = RuntimeCall::Proxy(pallet_proxy::Call::<Runtime>::add_proxy {
		delegate: ALICE.into(),
		proxy_type: polkadot_runtime::TransparentProxyType(
			polkadot_runtime_constants::proxy::ProxyType::Any,
		),
		delay: 0,
	});
	assert_closes_at_migration_start(add);

	let announce = RuntimeCall::Proxy(pallet_proxy::Call::<Runtime>::announce {
		real: ALICE.into(),
		call_hash: Default::default(),
	});
	assert_closes_at_migration_start(announce);

	let poke = RuntimeCall::Proxy(pallet_proxy::Call::<Runtime>::poke_deposit {});
	assert_closes_at_migration_start(poke);

	// The wrapper stays dispatchable throughout.
	let use_proxy = RuntimeCall::Proxy(pallet_proxy::Call::<Runtime>::proxy {
		real: ALICE.into(),
		force_proxy_type: None,
		call: Box::new(RuntimeCall::System(frame_system::Call::<Runtime>::remark {
			remark: vec![],
		})),
	});
	for stage in every_stage() {
		assert!(allowed_at(&stage, &use_proxy), "using a proxy must survive {stage:?}");
	}
}

/// The parachain control plane, which closes at the migration's first block rather than at the
/// runtime upgrade.
#[test]
fn the_parachain_control_plane_closes_when_the_migration_starts() {
	for call in [
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::reserve {}),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::deregister { id: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::add_lock { para: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::set_current_head {
			para: 2000.into(),
			new_head: HeadData(vec![1, 2, 3]),
		}),
		RuntimeCall::Hrmp(hrmp::Call::<Runtime>::hrmp_init_open_channel {
			recipient: 2001.into(),
			proposed_max_capacity: 8,
			proposed_max_message_size: 1024,
		}),
		RuntimeCall::Hrmp(hrmp::Call::<Runtime>::hrmp_close_channel {
			channel_id: HrmpChannelId { sender: 2000.into(), recipient: 2001.into() },
		}),
		RuntimeCall::Hrmp(hrmp::Call::<Runtime>::establish_channel_with_system {
			target_system_chain: ASSET_HUB_ID.into(),
		}),
	] {
		assert_closes_at_migration_start(call);
	}
}

/// The filter is a list, not a mode: a call it does not name is unaffected at every stage.
#[test]
fn calls_the_migration_does_not_name_are_untouched() {
	let call = RuntimeCall::System(frame_system::Call::<Runtime>::remark { remark: vec![1, 2, 3] });
	for stage in every_stage() {
		assert!(allowed_at(&stage, &call), "{call:?} must be unaffected at {stage:?}");
	}
}

/// The executor's teleport trust, which a call filter cannot cover: an inbound
/// `ReceiveTeleportedAsset` dispatches nothing on this chain.
mod inbound_teleports {
	use super::*;

	fn teleport_from_asset_hub(stage: &Stage) -> Outcome {
		new_test_ext().execute_with(|| {
			RcMigrationStage::<Runtime>::put(stage.clone());
			let message = Xcm::<RuntimeCall>(vec![
				ReceiveTeleportedAsset(Assets::from(vec![(Here, 10 * UNITS).into()])),
				DepositAsset {
					assets: AllCounted(1).into(),
					beneficiary: Location::new(
						0,
						[AccountId32 { network: None, id: ALICE.into() }],
					),
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
	fn a_system_chain_can_teleport_here_until_the_migration_starts() {
		for stage in open_stages() {
			assert_eq!(
				teleport_from_asset_hub(&stage).ensure_complete(),
				Ok(()),
				"teleports must still land at {stage:?}"
			);
		}
	}

	#[test]
	fn no_one_can_teleport_here_once_the_migration_starts() {
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
}
