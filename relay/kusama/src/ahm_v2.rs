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
	AccountId, AccumulateForwardPalletId, BrokerId, OnDemandPalletId, Runtime, RuntimeEvent,
	SocietyPalletId, Timestamp, TreasuryPalletId,
};
use alloc::{vec, vec::Vec};
use frame_support::{parameter_types, traits::Equals};
use frame_system::EnsureRoot;
use pallet_xcm::EnsureXcm;
use sp_runtime::traits::AccountIdConversion;

parameter_types! {
	/// Leftover pots emptied by the migration's `Sweep` stage.
	///
	/// Kusama's list is not Polkadot's: there is no retired direct-allocation pot here, and the
	/// Society pot is Kusama-only. Each entry is a pot whose balance has no owner to migrate it
	/// to, so it is swept rather than left stranded on a chain that will hold no KSM.
	pub SweepAccounts: Vec<AccountId> = vec![
		TreasuryPalletId::get().into_account_truncating(),
		SocietyPalletId::get().into_account_truncating(),
		OnDemandPalletId::get().into_account_truncating(),
		// The accumulate-and-forward pot that collects relay-chain dust for Asset Hub.
		AccumulateForwardPalletId::get().into_account_truncating(),
	];
	/// Where swept pots and dust land on Asset Hub: the AH treasury account (same `PalletId`
	/// derivation, so the same address).
	pub SweepBeneficiary: AccountId = TreasuryPalletId::get().into_account_truncating();
}

impl pallet_rc2_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = XcmRouter;
	type CtParaId = BrokerId;
	type TimeProvider = Timestamp;
	type CtOrigin = EnsureXcm<Equals<Broker>>;
	type AdminOrigin = EnsureRoot<AccountId>;
	type SweepAccounts = SweepAccounts;
	type SweepBeneficiary = SweepBeneficiary;
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
