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

use crate::{xcm_config::XcmRouter, AccountId, Balances, Runtime, RuntimeEvent, RuntimeHoldReason};
use frame_system::EnsureRoot;

impl pallet_ct_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type AdminOrigin = EnsureRoot<AccountId>;
	type Currency = Balances;
	type RuntimeHoldReason = RuntimeHoldReason;
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

	fn allowed_at(stage: MigrationStage, call: &RuntimeCall) -> bool {
		ExtBuilder::<Runtime>::default().build().execute_with(|| {
			CtMigrationStage::<Runtime>::put(stage);
			<Runtime as frame_system::Config>::BaseCallFilter::contains(call)
		})
	}

	/// Proxy definitions arrive from the relay chain while the migration runs, so the proxy map is
	/// closed until the relay chain ends the lockdown. Using a proxy is not.
	#[test]
	fn proxy_changes_close_while_the_migration_runs() {
		let alice = AccountId::new([1u8; 32]);
		let add_proxy = RuntimeCall::Proxy(pallet_proxy::Call::add_proxy {
			delegate: alice.clone().into(),
			proxy_type: ProxyType::Any,
			delay: 0,
		});
		let use_proxy = RuntimeCall::Proxy(pallet_proxy::Call::proxy {
			real: alice.into(),
			force_proxy_type: None,
			call: Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![] })),
		});

		assert!(allowed_at(MigrationStage::Pending, &add_proxy));
		assert!(!allowed_at(MigrationStage::DataMigrationOngoing, &add_proxy));
		assert!(allowed_at(MigrationStage::MigrationDone, &add_proxy));

		for stage in [
			MigrationStage::Pending,
			MigrationStage::DataMigrationOngoing,
			MigrationStage::MigrationDone,
		] {
			assert!(allowed_at(stage.clone(), &use_proxy), "using a proxy must work at {stage:?}");
		}
	}
}
