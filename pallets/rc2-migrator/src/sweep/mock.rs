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

//! Mock relay-chain runtime for the sweep stage.

use crate as pallet_rc2_migrator;
use crate::{sweep::MigratedBalances, RcMigratedBalance};
use frame_support::{derive_impl, parameter_types, traits::Currency};
use sp_runtime::{traits::IdentityLookup, AccountId32, BuildStorage};

type Block = frame_system::mocking::MockBlockU32<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Rc2Migrator: pallet_rc2_migrator,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountId = AccountId32;
	type Lookup = IdentityLookup<AccountId32>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u128>;
}

/// The relay chain's existential deposit.
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

parameter_types! {
	pub static SweepAccounts: Vec<AccountId32> = vec![pot()];
	pub SweepBeneficiary: AccountId32 = acc(200);
}

impl pallet_rc2_migrator::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type SweepAccounts = SweepAccounts;
	type SweepBeneficiary = SweepBeneficiary;
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

/// A pallet (module) account: the kind the migration leaves for the sweep stage.
pub fn pot() -> AccountId32 {
	let mut bytes = [0u8; 32];
	bytes[..12].copy_from_slice(b"modlpy/trsry");
	AccountId32::new(bytes)
}

pub fn fund(who: &AccountId32, amount: u128) {
	let _ = <Balances as Currency<AccountId32>>::make_free_balance_be(who, amount);
}

pub fn free(who: &AccountId32) -> u128 {
	pallet_balances::Pallet::<Test>::free_balance(who)
}

pub fn total_issuance() -> u128 {
	pallet_balances::TotalIssuance::<Test>::get()
}

pub fn exists(who: &AccountId32) -> bool {
	frame_system::Account::<Test>::contains_key(who)
}

/// Seed the conservation ledger the way the accounts stage does before anything moves.
pub fn seed_ledger() {
	RcMigratedBalance::<Test>::put(MigratedBalances {
		kept: total_issuance(),
		..Default::default()
	});
}

/// All `pallet-rc2-migrator` events since the last call to this function.
pub fn migrator_events() -> Vec<crate::Event<Test>> {
	let events = System::events()
		.into_iter()
		.filter_map(|r| match r.event {
			RuntimeEvent::Rc2Migrator(e) => Some(e),
			_ => None,
		})
		.collect();
	System::reset_events();
	events
}

/// Create the below-ED / broken-refcount account shapes that exist on chain but cannot be
/// produced through the balances API (it refuses sub-ED accounts).
pub fn force_anomalous_account(who: &AccountId32, free: u128, reserved: u128, consumers: u32) {
	let _ = frame_system::Pallet::<Test>::inc_providers(who);
	frame_system::Account::<Test>::mutate(who, |a| {
		a.data.free = free;
		a.data.reserved = reserved;
	});
	for _ in 0..consumers {
		frame_system::Pallet::<Test>::inc_consumers(who).unwrap();
	}
	pallet_balances::TotalIssuance::<Test>::mutate(|ti| *ti += free + reserved);
}
