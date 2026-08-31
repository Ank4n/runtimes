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

//! What `PostAhmFilter` closes, and what it must leave open.
//!
//! The registrar and HRMP entry points moved to the Coretime chain, so this chain has to refuse
//! the calls it used to serve while still accepting the requests Coretime sends back. Both halves
//! matter: block too little and two control planes run at once and diverge; block too much and
//! every parachain flow stops, silently — a call filtered inside XCM surfaces only as a `Transact`
//! that did nothing.

use frame_support::traits::Contains;
use pallet_rc2_migrator::{MigrationStage, RcMigrationStage};
use polkadot_primitives::HrmpChannelId;
use polkadot_runtime::{PostAhmFilter, Runtime, RuntimeCall};
use polkadot_runtime_common::paras_registrar;
use runtime_parachains::hrmp;
use sp_runtime::{AccountId32, BuildStorage};

type Stage = MigrationStage<AccountId32, u32>;

/// The filter reads the migration stage, so it needs storage. `Pending` is the default, and is
/// what a fresh runtime upgrade lands in.
fn allowed_at(stage: Stage, call: &RuntimeCall) -> bool {
	let mut ext: sp_io::TestExternalities =
		frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap().into();
	ext.execute_with(|| {
		RcMigrationStage::<Runtime>::put(stage);
		PostAhmFilter::contains(call)
	})
}


/// The registrar's own calls: served until the migration starts, closed from then on.
///
/// The upgrade must not be the cut-off. The Coretime pallets hold nothing until the migration
/// hands state over, so closing these at the upgrade would leave users with no registrar on
/// either chain for as long as governance takes to schedule the start. Root is unaffected
/// throughout: it bypasses the base call filter entirely, which is how governance and the
/// migration still reach these.
///
/// The calls a parachain dispatches **for itself** follow a three-phase schedule rather than
/// closing for good, because after the migration their bodies no longer touch this chain — they
/// forward the request to Coretime on the para's behalf. Keeping them open is what lets every
/// parachain go on encoding exactly the call it encodes today.
///
/// The middle phase is the load-bearing one. The forwarder turns on when the migration is
/// **finished**, so while it is running these calls would still take the local path and act on a
/// half-drained registry. They must be shut for exactly that window and no longer.
#[test]
fn para_facing_calls_reopen_as_forwarders_once_the_migration_is_done() {
	for call in [
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::deregister { id: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::add_lock { para: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::remove_lock { para: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::set_current_head {
			para: 2000.into(),
			new_head: polkadot_primitives::HeadData(vec![1, 2, 3]),
		}),
		RuntimeCall::Hrmp(hrmp::Call::<Runtime>::hrmp_init_open_channel {
			recipient: 2001.into(),
			proposed_max_capacity: 8,
			proposed_max_message_size: 1024,
		}),
		RuntimeCall::Hrmp(hrmp::Call::<Runtime>::hrmp_accept_open_channel { sender: 2000.into() }),
		RuntimeCall::Hrmp(hrmp::Call::<Runtime>::hrmp_close_channel {
			channel_id: HrmpChannelId { sender: 2000.into(), recipient: 2001.into() },
		}),
		RuntimeCall::Hrmp(hrmp::Call::<Runtime>::hrmp_cancel_open_request {
			channel_id: HrmpChannelId { sender: 2000.into(), recipient: 2001.into() },
			open_requests: 1,
		}),
		RuntimeCall::Hrmp(hrmp::Call::<Runtime>::establish_channel_with_system {
			target_system_chain: 1000.into(),
		}),
	] {
		// Before the migration begins: served locally, exactly as today.
		assert!(allowed_at(Stage::Pending, &call), "{call:?} must survive the upgrade");
		assert!(
			allowed_at(Stage::Scheduled { start: 100 }, &call),
			"{call:?} must stay open until the migration actually begins"
		);

		// While it runs: shut. The forwarder is not on yet and the registry is half drained.
		for stage in [Stage::RegistrarInit, Stage::HrmpInit, Stage::Paused] {
			assert!(
				!allowed_at(stage.clone(), &call),
				"{call:?} must be closed while the migration runs, at {stage:?}"
			);
		}

		// Afterwards: open again, now forwarding to Coretime.
		assert!(
			allowed_at(Stage::MigrationDone, &call),
			"{call:?} must reopen as a forwarder once the migration is done"
		);
	}
}

/// The calls that cannot be forwarded, and so close for good.
///
/// `reserve` and `swap` have no Coretime counterpart a para may drive — ids are allocated there and
/// swap is retired. `schedule_code_upgrade` carries the whole validation code, which is precisely
/// what the Coretime protocol refuses to move: it commits to a hash and a length and has the blob
/// uploaded separately. A parachain's ordinary upgrade path is `parachain_system`'s
/// `authorize_upgrade`, which never touches this pallet.
#[test]
fn calls_that_cannot_be_forwarded_stay_closed() {
	for call in [
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::reserve {}),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::swap {
			id: 2000.into(),
			other: 2001.into(),
		}),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::schedule_code_upgrade {
			para: 2000.into(),
			new_code: polkadot_primitives::ValidationCode(vec![1; 32]),
		}),
	] {
		assert!(allowed_at(Stage::Pending, &call), "{call:?} must survive the upgrade");
		assert!(
			allowed_at(Stage::Scheduled { start: 100 }, &call),
			"{call:?} must stay open until the migration begins"
		);
		for stage in [Stage::RegistrarInit, Stage::Paused, Stage::MigrationDone] {
			assert!(!allowed_at(stage.clone(), &call), "{call:?} must be closed at {stage:?}");
		}
	}
}

/// `PostAhmFilter` closes the calls this chain used to serve; this closes the *origin* those calls
/// would have arrived with. The two are complementary and neither substitutes for the other: a
/// `Contains<RuntimeCall>` cannot see who is calling, so without this a non-system parachain could
/// still reach any relay-chain pallet nobody thought to filter.
mod origins {
	use frame_support::traits::EnsureOrigin;
	use runtime_parachains::origin as parachains_origin;
	use polkadot_runtime::{
		para_control::EnsureAnyParaSelf, xcm_config::SystemChildParachainAsNative, RuntimeOrigin,
	};
	use polkadot_runtime_constants::system_parachain::{ASSET_HUB_ID, BROKER_ID};
	use xcm::latest::prelude::*;
	use xcm_executor::traits::ConvertOrigin;

	fn native_origin(para: u32) -> Result<RuntimeOrigin, Location> {
		SystemChildParachainAsNative::convert_origin(
			Location::new(0, [Parachain(para)]),
			OriginKind::Native,
		)
	}

	#[test]
	fn system_parachains_keep_their_own_origin() {
		// Coretime drives the parachain control plane and Asset Hub drives staking; both reach
		// this chain as `Origin::Parachain(id)`, not as Root.
		for para in [BROKER_ID, ASSET_HUB_ID] {
			assert!(native_origin(para).is_ok(), "system para {para} must keep its origin");
		}
	}

	#[test]
	fn ordinary_parachains_get_the_narrow_control_plane_origin() {
		// Not `parachains_origin::Origin::Parachain`, which eleven calls accept — the control
		// plane's own origin, which is accepted by the nine calls a para dispatches for itself and
		// by nothing else. Two properties, and the second is the one that matters:
		for para in [2000u32, 2004, 3367] {
			let origin = native_origin(para).unwrap_or_else(|_| {
				panic!("para {para} must dispatch here as itself, or it cannot reach the forwarders")
			});

			// It resolves to the para, for the calls that accept it.
			assert_eq!(
				<EnsureAnyParaSelf as EnsureOrigin<RuntimeOrigin>>::try_origin(origin.clone())
					.ok(),
				Some(para.into()),
				"para {para} must resolve through the control-plane origin"
			);

			// And it is *not* the system-chain origin, so nothing that accepts only that can be
			// reached with it — including a pallet nobody remembered to filter.
			assert!(
				<RuntimeOrigin as Into<Result<parachains_origin::Origin, RuntimeOrigin>>>::into(
					origin
				)
				.is_err(),
				"para {para} must not obtain the system-chain parachain origin"
			);
		}
	}
}
