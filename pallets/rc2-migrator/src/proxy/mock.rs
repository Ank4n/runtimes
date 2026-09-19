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

//! Mock relay-chain runtime for the proxy stage: the real proxy pallet, so every entry in the
//! tests is placed the way mainnet placed it.

use crate as pallet_rc2_migrator;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	derive_impl, parameter_types,
	traits::{Currency, InstanceFilter, ReservableCurrency},
};
use polkadot_parachain_primitives::primitives::Id as ParaId;
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{AccountIdConversion, BlakeTwo256, IdentityLookup},
	AccountId32, BuildStorage,
};

type Block = frame_system::mocking::MockBlockU32<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Proxy: pallet_proxy,
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

/// Relay-side proxy permissions: two portable ones and one (`Staking`) that the destination does
/// not represent, mirroring the production `TryFrom` split.
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
	Staking,
}

impl TryFrom<ProxyType> for migrator_types::PortableProxyType {
	type Error = ();

	fn try_from(t: ProxyType) -> Result<Self, ()> {
		use migrator_types::PortableProxyType as P;
		match t {
			ProxyType::Any => Ok(P::Any),
			ProxyType::NonTransfer => Ok(P::NonTransfer),
			ProxyType::Staking => Err(()),
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
	pub const ProxyDepositBase: u128 = 40;
	pub const ProxyDepositFactor: u128 = 4;
	pub const AnnouncementDepositBase: u128 = 25;
	pub const AnnouncementDepositFactor: u128 = 6;
	pub const MaxProxies: u16 = 4;
	pub const MaxPending: u16 = 4;
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

impl pallet_rc2_migrator::Config for Test {
	type RuntimeEvent = RuntimeEvent;
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

/// The child sovereign account of a para on the relay chain (`para…`).
pub fn child_sov(para: u32) -> AccountId32 {
	ParaId::from(para).into_account_truncating()
}

pub fn fund(who: &AccountId32, amount: u128) {
	let _ = <Balances as Currency<AccountId32>>::make_free_balance_be(who, amount);
}

pub fn free(who: &AccountId32) -> u128 {
	pallet_balances::Pallet::<Test>::free_balance(who)
}

pub fn reserved(who: &AccountId32) -> u128 {
	pallet_balances::Pallet::<Test>::reserved_balance(who)
}

/// Grant a proxy through the real pallet path; reserves the deposit at this chain's rates.
pub fn add_proxy(
	delegator: &AccountId32,
	delegate: &AccountId32,
	proxy_type: ProxyType,
	delay: u32,
) {
	Proxy::add_proxy(RuntimeOrigin::signed(delegator.clone()), delegate.clone(), proxy_type, delay)
		.expect("can add proxy");
}

/// What the accounts stage leaves of a delegator whose balance migrated: nothing.
pub fn withdraw(who: &AccountId32) {
	let reserved = reserved(who);
	<Balances as ReservableCurrency<AccountId32>>::unreserve(who, reserved);
	let _ = <Balances as Currency<AccountId32>>::make_free_balance_be(who, 0);
	assert!(!frame_system::Account::<Test>::contains_key(who), "account is reaped");
}

/// What the accounts stage leaves of a delegator that a consumer reference forbids reaping (a
/// session-key holder): the record, with nothing in it.
pub fn drain_to_shell(who: &AccountId32) {
	frame_system::Pallet::<Test>::inc_consumers(who).unwrap();
	let total = free(who) + reserved(who);
	frame_system::Account::<Test>::mutate(who, |a| {
		a.data.free = 0;
		a.data.reserved = 0;
	});
	pallet_balances::TotalIssuance::<Test>::mutate(|ti| *ti -= total);
}

/// Create a below-ED account, which the balances API refuses to produce.
pub fn force_dust_account(who: &AccountId32, free: u128) {
	let _ = frame_system::Pallet::<Test>::inc_providers(who);
	frame_system::Account::<Test>::mutate(who, |a| a.data.free = free);
	pallet_balances::TotalIssuance::<Test>::mutate(|ti| *ti += free);
}
