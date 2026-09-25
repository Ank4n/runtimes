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
	xcm_config::{Broker, XcmRouter},
	AccountId, Balance, BrokerId, ProxyType, Runtime, RuntimeEvent, Timestamp,
	TransparentProxyType,
};
use frame_support::{parameter_types, traits::Equals};
use frame_system::EnsureRoot;
use kusama_runtime_constants::{
	currency::{EXISTENTIAL_DEPOSIT, UNITS},
	system_parachain::ASSET_HUB_ID,
};
use migrator_types::PortableProxyType;
use pallet_xcm::EnsureXcm;

parameter_types! {
	/// Para id of Asset Hub, where teleported free balances land.
	pub const AhParaId: u32 = ASSET_HUB_ID;
	/// Working buffer of free balance that follows a migrated deposit to the Coretime chain.
	pub const CtFreeBuffer: Balance = UNITS;
	/// Asset Hub's existential deposit; mirrors
	/// `system_parachains_constants::kusama::currency::SYSTEM_PARA_EXISTENTIAL_DEPOSIT`
	/// (= relay ED / 10) without pulling that crate into the relay runtime.
	pub const AhExistentialDeposit: Balance = EXISTENTIAL_DEPOSIT / 10;
}

impl pallet_rc2_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type CtParaId = BrokerId;
	type AhParaId = AhParaId;
	type TimeProvider = Timestamp;
	type CtOrigin = EnsureXcm<Equals<Broker>>;
	type AdminOrigin = EnsureRoot<AccountId>;
	type CtFreeBuffer = CtFreeBuffer;
	type AhExistentialDeposit = AhExistentialDeposit;
}

/// Which proxy permissions travel to the Coretime chain in the migration. Permissions with no
/// meaning there (staking, governance, society, …) return `Err` and their definitions stay on
/// this chain.
impl TryFrom<TransparentProxyType> for PortableProxyType {
	type Error = ();

	fn try_from(t: TransparentProxyType) -> Result<Self, ()> {
		match t.0 {
			ProxyType::Any => Ok(PortableProxyType::Any),
			ProxyType::NonTransfer => Ok(PortableProxyType::NonTransfer),
			ProxyType::CancelProxy => Ok(PortableProxyType::CancelProxy),
			ProxyType::ParaRegistration => Ok(PortableProxyType::ParaRegistration),
			ProxyType::Governance |
			ProxyType::Staking |
			ProxyType::Auction |
			ProxyType::Society |
			ProxyType::Spokesperson |
			ProxyType::NominationPools => Err(()),
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::{Runtime, RuntimeCall};
	use codec::Encode;
	use pallet_ct_migrator::{Rc2MigratorCall, Rc2RuntimeCall};

	/// Ensure the pallet + call index aligns.
	#[test]
	fn the_coretime_chain_encodes_this_chains_calls_correctly() {
		assert_eq!(
			Rc2RuntimeCall::Rc2Migrator(Rc2MigratorCall::CtReady).encode(),
			RuntimeCall::Rc2Migrator(pallet_rc2_migrator::Call::<Runtime>::ct_ready {}).encode(),
		);
	}
}
