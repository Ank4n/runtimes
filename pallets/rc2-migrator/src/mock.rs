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

//! Test runtime for `pallet-rc2-migrator`.

use crate as pallet_rc2_migrator;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	derive_impl, ord_parameter_types, parameter_types,
	traits::{Currency, InstanceFilter, OnInitialize, ReservableCurrency, Time},
};
use frame_system::EnsureSignedBy;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{AccountIdConversion, BlakeTwo256, IdentityLookup},
	AccountId32, BuildStorage,
};
use xcm::prelude::*;

type Block = frame_system::mocking::MockBlock<Test>;
pub type AccountId = AccountId32;

// The proxy pallet is the real one, so every entry in the tests is placed the way mainnet placed
// it.
frame_support::construct_runtime! {
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Proxy: pallet_proxy,
		Rc2Migrator: pallet_rc2_migrator,
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountId = AccountId;
	type Lookup = IdentityLookup<AccountId>;
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

/// The account the mock treats as the Coretime chain's dispatch origin.
pub const CORETIME: AccountId = AccountId32::new([5; 32]);

/// Somebody
pub const ALICE: AccountId = AccountId32::new([1; 32]);

/// The account behind the admin origin.
pub const ADMIN: AccountId = AccountId32::new([2; 32]);

pub const CT_PARA_ID: u32 = 1005;
pub const WARM_UP: u64 = 4;
pub const COOL_OFF: u64 = 10;
/// Relay-chain block time
pub const BLOCK_TIME_MS: u64 = 6_000;

parameter_types! {
	pub const CtParaId: u32 = CT_PARA_ID;

	/// Every message the pallet successfully sent, in order.
	pub static SentXcm: Vec<(Location, Xcm<()>)> = vec![];
	/// Makes the router reject everything, to exercise the retry-next-block path.
	pub static SendFails: bool = false;

	/// The mock wall clock, in milliseconds. Advanced by [`run_blocks`].
	pub static MockNow: u64 = BLOCK_TIME_MS;
}

/// Stands in for `pallet_timestamp`.
pub struct MockTime;

impl Time for MockTime {
	type Moment = u64;

	fn now() -> Self::Moment {
		MockNow::get()
	}
}

/// Records what the pallet sends instead of delivering it.
pub struct RecordingRouter;

impl SendXcm for RecordingRouter {
	type Ticket = (Location, Xcm<()>);

	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<Self::Ticket> {
		if SendFails::get() {
			return Err(SendError::Transport("send forced to fail"));
		}
		let ticket =
			(dest.take().expect("destination is set"), msg.take().expect("message is set"));
		Ok((ticket, Assets::new()))
	}

	fn deliver(ticket: Self::Ticket) -> Result<XcmHash, SendError> {
		let mut sent = SentXcm::get();
		sent.push(ticket);
		SentXcm::set(sent);
		Ok([0; 32])
	}
}

ord_parameter_types! {
	pub const CoretimeAccount: AccountId = CORETIME;
	pub const AdminAccount: AccountId = ADMIN;
}

impl pallet_rc2_migrator::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = RecordingRouter;
	type CtParaId = CtParaId;
	type TimeProvider = MockTime;
	type CtOrigin = EnsureSignedBy<CoretimeAccount, AccountId>;
	type AdminOrigin = EnsureSignedBy<AdminAccount, AccountId>;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	SentXcm::set(vec![]);
	SendFails::set(false);
	MockNow::set(BLOCK_TIME_MS);

	let storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(storage);
	// Block 0 does not record events.
	ext.execute_with(|| System::set_block_number(1));
	ext
}

/// Run the next `n` blocks, advancing the clock like a real chain does.
///
/// The clock moves *after* the block's hooks, mirroring the timestamp inherent, which is an
/// extrinsic and so runs after `on_initialize`.
pub fn run_blocks(n: u64) {
	for _ in 0..n {
		let now = System::block_number() + 1;
		System::set_block_number(now);
		<Rc2Migrator as OnInitialize<u64>>::on_initialize(now);
		MockNow::set(now.saturating_mul(BLOCK_TIME_MS));
	}
}

/// The mock clock's current value.
pub fn now_ms() -> u64 {
	MockTime::now()
}

/// The messages sent so far, destination and all.
pub fn sent() -> Vec<(Location, Xcm<()>)> {
	SentXcm::get()
}

/// Decode the `Transact` payload of the `n`th sent message as a Coretime runtime call.
pub fn sent_call(n: usize) -> crate::CtRuntimeCall {
	let (_, Xcm(instructions)) = sent().get(n).expect("message was sent").clone();
	for instruction in instructions {
		if let Instruction::Transact { call, .. } = instruction {
			return crate::CtRuntimeCall::decode(&mut &call.into_encoded()[..])
				.expect("payload decodes as a Coretime call");
		}
	}
	panic!("message {n} carried no Transact");
}

pub fn acc(n: u8) -> AccountId {
	AccountId32::new([n; 32])
}

/// The child sovereign account of a para on the relay chain (`para…`).
pub fn child_sov(para: u32) -> AccountId {
	ParaId::from(para).into_account_truncating()
}

pub fn fund(who: &AccountId, amount: u128) {
	let _ = <Balances as Currency<AccountId>>::make_free_balance_be(who, amount);
}

pub fn free(who: &AccountId) -> u128 {
	pallet_balances::Pallet::<Test>::free_balance(who)
}

pub fn reserved(who: &AccountId) -> u128 {
	pallet_balances::Pallet::<Test>::reserved_balance(who)
}

/// Grant a proxy through the real pallet path; reserves the deposit at this chain's rates.
pub fn add_proxy(delegator: &AccountId, delegate: &AccountId, proxy_type: ProxyType, delay: u32) {
	Proxy::add_proxy(
		RuntimeOrigin::signed(delegator.clone()),
		delegate.clone(),
		proxy_type,
		delay.into(),
	)
	.expect("can add proxy");
}

/// What the accounts stage leaves of a delegator whose balance migrated: nothing.
pub fn withdraw(who: &AccountId) {
	let reserved = reserved(who);
	<Balances as ReservableCurrency<AccountId>>::unreserve(who, reserved);
	let _ = <Balances as Currency<AccountId>>::make_free_balance_be(who, 0);
	assert!(!frame_system::Account::<Test>::contains_key(who), "account is reaped");
}

/// What the accounts stage leaves of a delegator that a consumer reference forbids reaping (a
/// session-key holder): the record, with nothing in it.
pub fn drain_to_shell(who: &AccountId) {
	frame_system::Pallet::<Test>::inc_consumers(who).unwrap();
	let total = free(who) + reserved(who);
	frame_system::Account::<Test>::mutate(who, |a| {
		a.data.free = 0;
		a.data.reserved = 0;
	});
	pallet_balances::TotalIssuance::<Test>::mutate(|ti| *ti -= total);
}

/// Create a below-ED account, which the balances API refuses to produce.
pub fn force_dust_account(who: &AccountId, free: u128) {
	let _ = frame_system::Pallet::<Test>::inc_providers(who);
	frame_system::Account::<Test>::mutate(who, |a| a.data.free = free);
	pallet_balances::TotalIssuance::<Test>::mutate(|ti| *ti += free);
}
