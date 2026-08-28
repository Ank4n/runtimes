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
use polkadot_runtime::{PostAhmFilter, Runtime, RuntimeCall};
use polkadot_runtime_common::paras_registrar;
use polkadot_primitives::HrmpChannelId;
use runtime_parachains::hrmp;

fn allowed(call: RuntimeCall) -> bool {
	PostAhmFilter::contains(&call)
}

/// The old user-facing registrar calls are closed. Root is unaffected: it bypasses the base call
/// filter entirely, which is how governance and the migration still reach these.
#[test]
fn registrar_calls_are_closed() {
	for call in [
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::reserve {}),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::deregister { id: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::add_lock { para: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::remove_lock { para: 2000.into() }),
		RuntimeCall::Registrar(paras_registrar::Call::<Runtime>::swap {
			id: 2000.into(),
			other: 2001.into(),
		}),
	] {
		assert!(!allowed(call.clone()), "{call:?} must not be reachable on the relay chain");
	}
}

/// The same for HRMP. This is the actor change: a parachain used to open a channel by
/// `Transact`ing this call with its own origin, and that origin is filtered like any other
/// non-Root one.
#[test]
fn hrmp_calls_are_closed() {
	for call in [
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
	] {
		assert!(!allowed(call.clone()), "{call:?} must not be reachable on the relay chain");
	}
}

/// Everything Coretime drives, which arrives as `Origin::Parachain(BROKER_ID)` — filtered like any
/// other non-Root origin, so this has to be an explicit allow.
#[test]
fn the_control_plane_stays_open() {
	let registrar_msg = registrar_primitives::MessageToRelay::V1(
		registrar_primitives::MessageToRelayV1::Deregister { para_id: 2000, message_id: 0 },
	);
	let hrmp_msg = hrmp_primitives::MessageToRelay::V1(
		hrmp_primitives::MessageToRelayV1::AcceptOpenChannel {
			channel: hrmp_primitives::ChannelId { sender: 2000, recipient: 2001 },
			message_id: 0,
		},
	);

	for call in [
		RuntimeCall::RegistrarRelay(pallet_registrar_relay::Call::<Runtime>::deregister {
			message: registrar_msg.clone(),
		}),
		RuntimeCall::RegistrarRelay(pallet_registrar_relay::Call::<Runtime>::authorize_code {
			message: registrar_msg,
		}),
		RuntimeCall::HrmpRelay(pallet_hrmp_relay::Call::<Runtime>::accept_open_channel {
			message: hrmp_msg.clone(),
		}),
		RuntimeCall::HrmpRelay(pallet_hrmp_relay::Call::<Runtime>::establish_system_channel {
			message: hrmp_msg,
		}),
	] {
		assert!(allowed(call.clone()), "{call:?} is how Coretime drives this chain");
	}
}

/// The validation-code uploads are unsigned and feeless, and unsigned is not Root — so they are
/// filtered too, and a registration that has been authorized would have no way to complete.
#[test]
fn the_unsigned_code_uploads_stay_open() {
	for call in [
		RuntimeCall::RegistrarRelay(pallet_registrar_relay::Call::<Runtime>::apply_authorized_code {
			para_id: 2000,
			validation_code: vec![1, 2, 3],
		}),
		RuntimeCall::RegistrarRelay(
			pallet_registrar_relay::Call::<Runtime>::apply_authorized_code_upgrade {
				para_id: 2000,
				validation_code: vec![1, 2, 3],
			},
		),
	] {
		assert!(allowed(call.clone()), "{call:?} is the only way code reaches this chain");
	}
}

/// The origin half of the same rule.
///
/// `PostAhmFilter` closes the calls this chain used to serve; this closes the *origin* those calls
/// would have arrived with. The two are complementary and neither substitutes for the other: a
/// `Contains<RuntimeCall>` cannot see who is calling, so without this a non-system parachain could
/// still reach any relay-chain pallet nobody thought to filter.
mod origins {
	use polkadot_runtime::{xcm_config::SystemChildParachainAsNative, RuntimeOrigin};
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
	fn ordinary_parachains_get_no_origin_at_all() {
		// Not "gets an origin that every pallet then refuses" — gets none, so the `Transact`
		// fails to convert before any call is even looked at.
		for para in [2000, 2004, 3367] {
			assert!(native_origin(para).is_err(), "para {para} must not dispatch here as itself");
		}
	}
}
