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

//! Mock receiving-chain runtime for the proxy stage: the real proxy pallet, so recreated entries
//! are priced the way the chain prices them.

use crate as pallet_ct_migrator;
use crate::{proxy::PortableProxyOf, HoldReason};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	derive_impl, parameter_types,
	traits::{
		fungible::{InspectHold, Mutate, Unbalanced, UnbalancedHold},
		tokens::{Fortitude, Precision, Preservation},
		ConstU32, InstanceFilter,
	},
};
use migrator_types::{PortableProxy, PortableProxyDelegate, PortableProxyType};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{BlakeTwo256, IdentityLookup, Zero},
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
	type RcBlockTimeRatio = ConstU32<2>;
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

pub fn reserved(who: &AccountId32) -> u128 {
	pallet_balances::Pallet::<Test>::reserved_balance(who)
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

/// The state the accounts stage leaves a migrated delegator in: `free` liquid and `hold` under
/// `ProxyDeposit`. Placed the way that stage places it — a provider reference for a sub-ED
/// `free`, and the hold booked before the free balance is decreased — so the dust survives.
pub fn give_proxy_deposit(who: &AccountId32, hold: u128, free: u128) {
	let reason = RuntimeHoldReason::CtMigrator(HoldReason::ProxyDeposit);
	if frame_system::Pallet::<Test>::providers(who).is_zero() && free < ED {
		frame_system::Pallet::<Test>::inc_providers(who);
	}
	<Balances as Mutate<AccountId32>>::mint_into(who, free + hold).unwrap();
	<Balances as UnbalancedHold<AccountId32>>::increase_balance_on_hold(
		&reason,
		who,
		hold,
		Precision::Exact,
	)
	.unwrap();
	<Balances as Unbalanced<AccountId32>>::decrease_balance(
		who,
		hold,
		Precision::Exact,
		Preservation::Expendable,
		Fortitude::Force,
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

pub fn portable_proxy(
	delegator: &AccountId32,
	delegates: Vec<(AccountId32, PortableProxyType, u32)>,
) -> PortableProxyOf<Test> {
	let delegates: Vec<_> = delegates
		.into_iter()
		.map(|(delegate, proxy_type, delay)| PortableProxyDelegate { delegate, proxy_type, delay })
		.collect();
	PortableProxy { delegator: delegator.clone(), delegates: delegates.try_into().unwrap() }
}
