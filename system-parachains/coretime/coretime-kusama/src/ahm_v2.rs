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
	xcm_config::XcmRouter, AccountId, Balances, ProxyType, Runtime, RuntimeEvent, RuntimeHoldReason,
};
use frame_support::traits::ConstU32;
use frame_system::EnsureRoot;
use migrator_types::PortableProxyType;
use system_parachains_constants::{
	kusama::consensus::RELAY_CHAIN_SLOT_DURATION_MILLIS, MILLISECS_PER_BLOCK,
};

impl pallet_ct_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type AdminOrigin = EnsureRoot<AccountId>;
	type Currency = Balances;
	type RuntimeHoldReason = RuntimeHoldReason;
	type RcBlocksPerLocalBlock =
		ConstU32<{ (MILLISECS_PER_BLOCK / RELAY_CHAIN_SLOT_DURATION_MILLIS as u64) as u32 }>;
}

/// What each migrated relay-chain proxy permission becomes locally.
impl From<PortableProxyType> for ProxyType {
	fn from(portable: PortableProxyType) -> Self {
		match portable {
			PortableProxyType::Any => ProxyType::Any,
			PortableProxyType::NonTransfer => ProxyType::NonTransfer,
			PortableProxyType::CancelProxy => ProxyType::CancelProxy,
			PortableProxyType::ParaRegistration => ProxyType::ParaRegistration,
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::{Runtime, RuntimeCall};
	use codec::Encode;
	use pallet_rc2_migrator::{CtMigratorCall, CtRuntimeCall};

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
}
