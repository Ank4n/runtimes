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

//! The relay chain's half of the parachain control plane.
//!
//! After the Minimal Relay migration this chain accepts no signed origins, so the user-facing
//! registrar and HRMP calls all start on the Coretime chain. What stays here is the part only this
//! chain can do — onboarding, lifecycles, PVF pre-checks, message routing — reached through
//! `pallet-registrar-relay` and `pallet-hrmp-relay`.
//!
//! Requests arrive from exactly one origin: the Coretime chain, as an XCM `Transact` converted to
//! `parachains_origin::Origin::Parachain`. Reports go back as `Transact` with
//! `OriginKind::Superuser`, which Coretime converts to Root — the origin both para-side pallets
//! accept as their `RelayOrigin`.

use alloc::{vec, vec::Vec};
use crate::{parachains_origin, Hrmp, Registrar, Runtime, RuntimeEvent, RuntimeOrigin};
use codec::Encode;
use frame_support::{parameter_types, traits::EnsureOrigin};
use polkadot_runtime_constants::system_parachain::BROKER_ID;
use sp_runtime::transaction_validity::TransactionPriority;
use xcm::latest::prelude::*;

parameter_types! {
	/// Where reports are sent, and the only para allowed to drive the control plane.
	pub ControlPlaneLocation: Location = Location::new(0, [Parachain(BROKER_ID)]);

	/// The largest head data this chain will hold while a registration waits for its code.
	///
	/// The bound that matters is the **product** with `MaxPendingRegistrations`: this is
	/// relay-chain state that no deposit here pays for, and an entry only leaves when the code
	/// lands or Coretime cancels. 100 x 1 MiB is the real commitment. Nothing expires on its own;
	/// governance drops an abandoned entry with `force_drop_pending`.
	pub const RegistrarMaxHeadDataSize: u32 = polkadot_primitives::MAX_HEAD_DATA_SIZE;
	pub const RegistrarMaxCodeSize: u32 = polkadot_primitives::MAX_CODE_SIZE;
	pub const MaxPendingRegistrations: u32 = 100;

	/// Uploading a registration's validation code is unsigned and feeless, so it needs a priority
	/// of its own. Below the top of the range, so it cannot crowd out inherents.
	pub const RegistrarUnsignedPriority: TransactionPriority = TransactionPriority::MAX / 2;
}

/// Accepts Root, or the Coretime chain.
///
/// Same shape as [`crate::EnsureAssetHub`]: match the parachain origin the XCM converter produced
/// and check the id. Root is kept so governance kept a way in.
pub struct EnsureCoretime;

impl EnsureOrigin<RuntimeOrigin> for EnsureCoretime {
	type Success = ();

	fn try_origin(o: RuntimeOrigin) -> Result<Self::Success, RuntimeOrigin> {
		match <RuntimeOrigin as Into<Result<parachains_origin::Origin, RuntimeOrigin>>>::into(
			o.clone(),
		) {
			Ok(parachains_origin::Origin::Parachain(id)) if id == BROKER_ID.into() => Ok(()),
			_ => Err(o),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin() -> Result<RuntimeOrigin, ()> {
		Ok(RuntimeOrigin::root())
	}
}

/// Calls on the Coretime chain, as this chain must encode them.
///
/// Audit: the outer indices are the pallet indices in the Coretime `construct_runtime!`, and the
/// inner ones are `#[pallet::call_index]` of `receive` in each pallet. Nothing here is checked by
/// the compiler, so `call_encoding` in the minimal-relay integration tests decodes what this chain
/// would send using the real Coretime `RuntimeCall` — which is why these types are public.
#[derive(Encode)]
pub enum CoretimeRuntimePallets {
	#[codec(index = 60)]
	RegistrarPara(RegistrarParaCalls),
	#[codec(index = 61)]
	HrmpPara(HrmpParaCalls),
}

#[derive(Encode)]
pub enum RegistrarParaCalls {
	#[codec(index = 3)]
	Receive(registrar_primitives::MessageToPara),
}

#[derive(Encode)]
pub enum HrmpParaCalls {
	#[codec(index = 4)]
	Receive(hrmp_primitives::MessageToPara),
}

/// Hand a `Transact` to the Coretime chain.
///
/// `OriginKind::Superuser` so it lands there as Root, which is what both para-side pallets accept.
/// A send failure is returned, not raised: for a report the right answer is always to carry on,
/// because this chain's own state is already correct and the parachain has its own way to ask
/// again.
fn send_to_coretime(call: Vec<u8>) -> Result<(), ()> {
	let message = Xcm(vec![
		Instruction::UnpaidExecution { weight_limit: WeightLimit::Unlimited, check_origin: None },
		Instruction::Transact {
			origin_kind: OriginKind::Superuser,
			fallback_max_weight: None,
			call: call.into(),
		},
	]);

	send_xcm::<crate::xcm_config::XcmRouter>(ControlPlaneLocation::get(), message)
		.map(|_| ())
		.map_err(|_| ())
}

/// The registrar half of the transport.
pub struct RegistrarReportToCoretime;

impl pallet_registrar_relay::SendToPara for RegistrarReportToCoretime {
	fn send(message: registrar_primitives::MessageToPara) -> Result<(), ()> {
		send_to_coretime(
			CoretimeRuntimePallets::RegistrarPara(RegistrarParaCalls::Receive(message)).encode(),
		)
	}
}

/// The HRMP half of the transport.
pub struct HrmpReportToCoretime;

impl pallet_hrmp_relay::SendToPara for HrmpReportToCoretime {
	fn send(message: hrmp_primitives::MessageToPara) -> Result<(), ()> {
		send_to_coretime(
			CoretimeRuntimePallets::HrmpPara(HrmpParaCalls::Receive(message)).encode(),
		)
	}
}

impl pallet_registrar_relay::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type ParaOrigin = EnsureCoretime;
	type SendToPara = RegistrarReportToCoretime;
	// The real registry. A registration driven from Coretime goes through the same `do_register`
	// a local one would, so it cannot bypass any rule this chain has — it only skips the deposit,
	// which is held on Coretime instead.
	type Registrar = Registrar;
	type MaxHeadDataSize = RegistrarMaxHeadDataSize;
	type MaxCodeSize = RegistrarMaxCodeSize;
	type MaxPendingRegistrations = MaxPendingRegistrations;
	type UnsignedPriority = RegistrarUnsignedPriority;
	type WeightInfo = ();
}

impl pallet_hrmp_relay::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type ParaOrigin = EnsureCoretime;
	type SendToPara = HrmpReportToCoretime;
	// The real HRMP pallet, driven deposit-free: the deposits live on Coretime now, so reserving
	// here would charge a para twice, against a sovereign account the migration has emptied.
	type Registry = Hrmp;
	type WeightInfo = ();
}
