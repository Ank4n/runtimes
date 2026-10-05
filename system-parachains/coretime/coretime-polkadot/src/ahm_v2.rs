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

//! AHM v2 migration wiring: the Coretime-chain side of receiving account, proxy, registrar and
//! HRMP state from the relay chain.
//!
//! Compiled only with the `ahm-v2` feature, which released runtimes do not enable. The
//! integration tests turn it on to drive the real runtime.

use crate::{
	xcm_config::XcmRouter, AccountId, Balances, Runtime, RuntimeCall, RuntimeEvent,
	RuntimeHoldReason,
};
use frame_support::traits::{Contains, Everything};
use frame_system::EnsureRoot;

impl pallet_ct_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type AdminOrigin = EnsureRoot<AccountId>;
	type Currency = Balances;
	type RuntimeHoldReason = RuntimeHoldReason;
	type PreMigrationCalls = CallsEnabledBeforeMigration;
	type IntraMigrationCalls = CallsEnabledDuringMigration;
	type PostMigrationCalls = Everything;
}

/// Contains all calls that are enabled before the migration starts.
pub struct CallsEnabledBeforeMigration;
impl Contains<RuntimeCall> for CallsEnabledBeforeMigration {
	fn contains(call: &RuntimeCall) -> bool {
		let (before, _during) = call_allowed_status(call);
		if !before {
			log::warn!("Call bounced by the filter before the migration: {call:?}");
		}
		before
	}
}

/// Contains all calls that are enabled during the migration.
pub struct CallsEnabledDuringMigration;
impl Contains<RuntimeCall> for CallsEnabledDuringMigration {
	fn contains(call: &RuntimeCall) -> bool {
		let (_before, during) = call_allowed_status(call);
		if !during {
			log::warn!("Call bounced by the filter during the migration: {call:?}");
		}
		during
	}
}

/// Return whether a call is enabled before and during the migration. Every call is enabled after
/// it.
///
/// During is from the relay chain's start signal until it ends the lockdown.
///
/// Proxy definitions arrive from the relay chain and are merged into `pallet_proxy`, so during the
/// migration every proxy call that changes the proxy map or an announcement is disabled. Using a
/// proxy stays enabled. Announcements are not migrated: the relay chain releases their deposits
/// and sends them to Asset Hub as free balance.
pub fn call_allowed_status(call: &RuntimeCall) -> (bool, bool) {
	use RuntimeCall::*;
	const ON: bool = true;
	const OFF: bool = false;

	match call {
		System(..) => (ON, ON),
		ParachainSystem(..) => (ON, ON),
		Timestamp(..) => (ON, ON),
		ParachainInfo(..) => (ON, ON),
		Balances(..) => (ON, ON),
		CollatorSelection(..) => (ON, ON),
		Session(..) => (ON, ON),
		XcmpQueue(..) => (ON, ON),
		PolkadotXcm(..) => (ON, ON),
		CumulusXcm(..) => (ON, ON),
		MessageQueue(..) => (ON, ON),
		Utility(..) => (ON, ON),
		Multisig(..) => (ON, ON),
		Proxy(pallet_proxy::Call::proxy { .. } | pallet_proxy::Call::proxy_announced { .. }) =>
			(ON, ON),
		Broker(..) => (ON, ON),
		CtMigrator(..) => (ON, ON),

		// TODO(ahm-v2): `RegistrarPara` and `HrmpPara` join as `(OFF, OFF)`.
		Proxy(..) => (ON, OFF),
	}
}

#[cfg(test)]
mod tests {
	use crate::{AccountId, ProxyType, Runtime, RuntimeCall};
	use codec::Encode;
	use frame_support::traits::Contains;
	use pallet_ct_migrator::{CtMigrationStage, MigrationStage};
	use pallet_rc2_migrator::{CtMigratorCall, CtRuntimeCall};
	use parachains_runtimes_test_utils::ExtBuilder;

	/// Ensure the pallet + call index aligns.
	#[test]
	fn the_relay_chain_encodes_this_chains_calls_correctly() {
		assert_eq!(
			CtRuntimeCall::CtMigrator(CtMigratorCall::StartMigration).encode(),
			RuntimeCall::CtMigrator(pallet_ct_migrator::Call::<Runtime>::start_migration {})
				.encode(),
		);
		assert_eq!(
			CtRuntimeCall::CtMigrator(CtMigratorCall::EndLockdown).encode(),
			RuntimeCall::CtMigrator(pallet_ct_migrator::Call::<Runtime>::end_lockdown {}).encode(),
		);
	}

	fn allowed_at(stage: &MigrationStage, call: &RuntimeCall) -> bool {
		ExtBuilder::<Runtime>::default().build().execute_with(|| {
			CtMigrationStage::<Runtime>::put(stage.clone());
			<Runtime as frame_system::Config>::BaseCallFilter::contains(call)
		})
	}

	#[test]
	fn proxy_changes_are_refused_while_the_migration_runs() {
		let alice = AccountId::new([1; 32]); // delegate
		let remark = RuntimeCall::System(frame_system::Call::remark { remark: vec![1] });
		let changes = [
			RuntimeCall::Proxy(pallet_proxy::Call::add_proxy {
				delegate: alice.clone().into(),
				proxy_type: ProxyType::Any,
				delay: 0,
			}),
			RuntimeCall::Proxy(pallet_proxy::Call::remove_proxies {}),
			RuntimeCall::Proxy(pallet_proxy::Call::create_pure {
				proxy_type: ProxyType::Any,
				delay: 0,
				index: 0,
			}),
			RuntimeCall::Proxy(pallet_proxy::Call::announce {
				real: alice.clone().into(),
				call_hash: Default::default(),
			}),
			RuntimeCall::Proxy(pallet_proxy::Call::poke_deposit {}),
		];
		let uses = [
			RuntimeCall::Proxy(pallet_proxy::Call::proxy {
				real: alice.clone().into(),
				force_proxy_type: None,
				call: Box::new(remark.clone()),
			}),
			RuntimeCall::Proxy(pallet_proxy::Call::proxy_announced {
				delegate: alice.clone().into(),
				real: alice.into(),
				force_proxy_type: None,
				call: Box::new(remark.clone()),
			}),
			remark,
		];

		// GIVEN the migration has not started or is done. THEN proxy changes pass.
		for stage in [MigrationStage::Pending, MigrationStage::MigrationDone] {
			for call in &changes {
				assert!(allowed_at(&stage, call), "{call:?} refused at {stage:?}");
			}
		}

		// GIVEN the migration is running. THEN proxy changes are refused.
		for call in &changes {
			assert!(
				!allowed_at(&MigrationStage::DataMigrationOngoing, call),
				"{call:?} allowed while the migration runs"
			);
		}

		// THEN using a proxy, and any other call, passes at every stage.
		for stage in [
			MigrationStage::Pending,
			MigrationStage::DataMigrationOngoing,
			MigrationStage::MigrationDone,
		] {
			for call in &uses {
				assert!(allowed_at(&stage, call), "{call:?} refused at {stage:?}");
			}
		}
	}
}
