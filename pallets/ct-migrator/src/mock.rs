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

//! Test runtime for `pallet-ct-migrator`.

use crate as pallet_ct_migrator;
use crate::{proxy::PortableProxyOf, HoldReason};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	derive_impl, ord_parameter_types, parameter_types,
	traits::{
		fungible::{InspectHold, Mutate, Unbalanced, UnbalancedHold},
		tokens::{Fortitude, Precision, Preservation},
		ConstU128, ConstU32, InstanceFilter,
	},
};
use frame_system::EnsureSignedBy;
use migrator_types::{PortableProxy, PortableProxyDelegate, PortableProxyType};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{BlakeTwo256, Zero},
	BuildStorage,
};
use xcm::prelude::*;

type Block = frame_system::mocking::MockBlock<Test>;
pub type AccountId = u64;

frame_support::construct_runtime! {
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Proxy: pallet_proxy,
		CtMigrator: pallet_ct_migrator,
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u128>;
}

/// Existential deposit of the receiving chain.
pub const ED: u128 = 10;

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type AccountStore = System;
	type ExistentialDeposit = ConstU128<ED>;
	type RuntimeHoldReason = RuntimeHoldReason;
}

/// Local proxy permissions.
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

// Somebody
pub const ALICE: AccountId = 1;
// The account behind the admin origin.
pub const ADMIN: AccountId = 2;

ord_parameter_types! {
	pub const AdminAccount: AccountId = ADMIN;
}

parameter_types! {
	/// Every message the pallet successfully sent, in order.
	pub static SentXcm: Vec<(Location, Xcm<()>)> = vec![];
	/// Makes the router reject everything.
	pub static SendFails: bool = false;
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

impl pallet_ct_migrator::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = RecordingRouter;
	type AdminOrigin = EnsureSignedBy<AdminAccount, AccountId>;
	type Currency = Balances;
	type RuntimeHoldReason = RuntimeHoldReason;
	type RcBlocksPerLocalBlock = ConstU32<2>;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	SentXcm::set(vec![]);
	SendFails::set(false);

	let storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(storage);
	// Block 0 does not record events.
	ext.execute_with(|| System::set_block_number(1));
	ext
}

/// The messages sent so far, destination and all.
pub fn sent() -> Vec<(Location, Xcm<()>)> {
	SentXcm::get()
}

/// Decode the `Transact` payload of the `n`th sent message as a relay-chain runtime call.
pub fn sent_call(n: usize) -> crate::Rc2RuntimeCall {
	let (_, Xcm(instructions)) = sent().get(n).expect("message was sent").clone();
	for instruction in instructions {
		if let Instruction::Transact { call, .. } = instruction {
			return crate::Rc2RuntimeCall::decode(&mut &call.into_encoded()[..])
				.expect("payload decodes as a relay-chain call");
		}
	}
	panic!("message {n} carried no Transact");
}

pub fn acc(n: u8) -> AccountId {
	n as AccountId
}

pub fn free(who: &AccountId) -> u128 {
	pallet_balances::Pallet::<Test>::free_balance(who)
}

pub fn reserved(who: &AccountId) -> u128 {
	pallet_balances::Pallet::<Test>::reserved_balance(who)
}

pub fn held(reason: HoldReason, who: &AccountId) -> u128 {
	<Balances as InspectHold<AccountId>>::balance_on_hold(
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
pub fn give_proxy_deposit(who: &AccountId, hold: u128, free: u128) {
	let reason = RuntimeHoldReason::CtMigrator(HoldReason::ProxyDeposit);
	if frame_system::Pallet::<Test>::providers(who).is_zero() && free < ED {
		frame_system::Pallet::<Test>::inc_providers(who);
	}
	<Balances as Mutate<AccountId>>::mint_into(who, free + hold).unwrap();
	<Balances as UnbalancedHold<AccountId>>::increase_balance_on_hold(
		&reason,
		who,
		hold,
		Precision::Exact,
	)
	.unwrap();
	<Balances as Unbalanced<AccountId>>::decrease_balance(
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
	delegator: &AccountId,
	delegates: Vec<(AccountId, PortableProxyType, u32)>,
) -> PortableProxyOf<Test> {
	let delegates: Vec<_> = delegates
		.into_iter()
		.map(|(delegate, proxy_type, delay)| PortableProxyDelegate { delegate, proxy_type, delay })
		.collect();
	PortableProxy { delegator: *delegator, delegates: delegates.try_into().unwrap() }
}
