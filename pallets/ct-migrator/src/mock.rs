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

//! Mock runtime for `pallet-ct-migrator` unit tests.

use crate as pallet_ct_migrator;
use crate::*;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	derive_impl, parameter_types,
	traits::{fungible::Mutate, ConstU32, InstanceFilter},
};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{BlakeTwo256, IdentityLookup},
	AccountId32, BuildStorage,
};

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Proxy: pallet_proxy,
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

/// Local proxy permissions. Mirrors the shape of the Coretime runtime's `ProxyType`: a total
/// `From<PortableProxyType>` because the wire only carries permissions this chain represents.
#[derive(
	Copy,
	Clone,
	Eq,
	PartialEq,
	Ord,
	PartialOrd,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Debug,
	MaxEncodedLen,
	TypeInfo,
	Default,
)]
pub enum ProxyType {
	#[default]
	Any,
	NonTransfer,
	CancelProxy,
	ParaRegistration,
}

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

impl InstanceFilter<RuntimeCall> for ProxyType {
	fn filter(&self, _c: &RuntimeCall) -> bool {
		matches!(self, ProxyType::Any)
	}
	fn is_superset(&self, o: &Self) -> bool {
		self == o || matches!(self, ProxyType::Any)
	}
}

parameter_types! {
	/// This chain's proxy deposit rates; deliberately different from any "relay" rate a test
	/// simulates so resize assertions can't pass by accident.
	pub const ProxyDepositBase: u128 = 100;
	pub const ProxyDepositFactor: u128 = 20;
	/// Small on purpose: lets tests exercise the merged-set overflow path cheaply.
	pub const MaxProxies: u16 = 4;
	pub const MaxPending: u16 = 4;
	pub const AnnouncementDepositBase: u128 = 50;
	pub const AnnouncementDepositFactor: u128 = 10;
}

impl pallet_proxy::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type Currency = Balances;
	type ProxyType = ProxyType;
	type ProxyDepositBase = ProxyDepositBase;
	type ProxyDepositFactor = ProxyDepositFactor;
	type MaxProxies = MaxProxies;
	type WeightInfo = ();
	type MaxPending = MaxPending;
	type CallHasher = BlakeTwo256;
	type AnnouncementDepositBase = AnnouncementDepositBase;
	type AnnouncementDepositFactor = AnnouncementDepositFactor;
	type BlockNumberProvider = System;
}

impl pallet_ct_migrator::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type RuntimeHoldReason = RuntimeHoldReason;
	// Sender block time 6s, this chain 12s: migrated delays halve.
	type RcBlockTimeRatio = ConstU32<2>;
}

/// What each migrated hold becomes locally; mirrors the Coretime runtime's mapping.
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
	use frame_support::traits::fungible::InspectHold;
	<Balances as InspectHold<AccountId32>>::balance_on_hold(
		&RuntimeHoldReason::CtMigrator(reason),
		who,
	)
}

pub fn total_issuance() -> u128 {
	pallet_balances::TotalIssuance::<Test>::get()
}

/// Fund `who` and place `amount` under the given migrated-hold reason — the state the accounts
/// stage leaves behind, which the record stages then re-attribute.
pub fn give_hold(who: &AccountId32, reason: HoldReason, amount: u128) {
	use frame_support::traits::fungible::MutateHold;
	<Balances as Mutate<AccountId32>>::mint_into(who, amount + ED).unwrap();
	<Balances as MutateHold<AccountId32>>::hold(
		&RuntimeHoldReason::CtMigrator(reason),
		who,
		amount,
	)
	.unwrap();
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
