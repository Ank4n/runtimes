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

//! Mock receiving-chain runtime for the accounts stage.

use crate as pallet_ct_migrator;
use crate::{accounts::PortableAccountOf, HoldReason};
use frame_support::{derive_impl, parameter_types, traits::fungible::InspectHold};
use migrator_types::{PortableAccount, PortableHold, PortableHoldReason};
use sp_runtime::{traits::IdentityLookup, AccountId32, BuildStorage};

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		CtMigrator: pallet_ct_migrator,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountId = AccountId32;
	type Lookup = IdentityLookup<AccountId32>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u128>;
}

/// Existential deposit of the receiving chain.
pub const ED: u128 = 10;

parameter_types! {
	pub const ExistentialDeposit: u128 = ED;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type AccountStore = System;
	type ExistentialDeposit = ExistentialDeposit;
}

impl pallet_ct_migrator::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type RuntimeHoldReason = RuntimeHoldReason;
}

/// What each migrated relay-chain hold becomes on this chain; the production runtimes carry
/// the same `match`.
impl From<PortableHoldReason> for RuntimeHoldReason {
	fn from(reason: PortableHoldReason) -> Self {
		match reason {
			PortableHoldReason::UnnamedReserve =>
				RuntimeHoldReason::CtMigrator(HoldReason::RcMigratedReserve),
			PortableHoldReason::ProxyDeposit =>
				RuntimeHoldReason::CtMigrator(HoldReason::ProxyDeposit),
			PortableHoldReason::UnattributedReserve =>
				RuntimeHoldReason::CtMigrator(HoldReason::UnattributedReserve),
		}
	}
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	// Block 1 so deposited events are recorded.
	ext.execute_with(|| System::set_block_number(1));
	ext
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

pub fn acc(n: u8) -> AccountId32 {
	AccountId32::new([n; 32])
}

pub fn free(who: &AccountId32) -> u128 {
	pallet_balances::Pallet::<Test>::free_balance(who)
}

pub fn held(reason: HoldReason, who: &AccountId32) -> u128 {
	<Balances as InspectHold<AccountId32>>::balance_on_hold(
		&RuntimeHoldReason::CtMigrator(reason),
		who,
	)
}

pub fn total_issuance() -> u128 {
	pallet_balances::TotalIssuance::<Test>::get()
}

/// All `pallet-ct-migrator` events since the last call to this function.
pub fn migrator_events() -> Vec<crate::Event<Test>> {
	let events = System::events()
		.into_iter()
		.filter_map(|r| match r.event {
			RuntimeEvent::CtMigrator(e) => Some(e),
			_ => None,
		})
		.collect();
	System::reset_events();
	events
}

pub fn portable_account(
	who: &AccountId32,
	free: u128,
	holds: Vec<(PortableHoldReason, u128)>,
) -> PortableAccountOf<Test> {
	let holds: Vec<_> = holds
		.into_iter()
		.map(|(reason, amount)| PortableHold { reason, amount })
		.collect();
	PortableAccount { who: who.clone(), free, holds: holds.try_into().unwrap() }
}
