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
	traits::{
		ConstU128, Currency, InstanceFilter, LockableCurrency, OnInitialize, ReservableCurrency,
		Time, WithdrawReasons,
	},
	PalletId,
};
use frame_system::{EnsureRoot, EnsureSignedBy};
use migrator_types::PortableProxyType;
use polkadot_parachain_primitives::primitives::{HrmpChannelId, Id as ParaId};
use polkadot_runtime_common::paras_registrar;
use runtime_parachains::{
	configuration, dmp, hrmp as parachains_hrmp, origin as parachains_origin, paras, shared,
};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{AccountIdConversion, BlakeTwo256, IdentityLookup},
	transaction_validity::TransactionPriority,
	AccountId32, BuildStorage,
};
use xcm::prelude::*;

type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
type Block = frame_system::mocking::MockBlock<Test>;
pub type AccountId = AccountId32;

// The deposit-owning pallets are the real ones, so every expected reserve in the tests is placed
// the way mainnet placed it.
frame_support::construct_runtime! {
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Configuration: configuration,
		ParasShared: shared,
		Parachains: paras,
		Dmp: dmp,
		Hrmp: parachains_hrmp,
		ParachainsOrigin: parachains_origin,
		Registrar: paras_registrar,
		Multisig: pallet_multisig,
		Proxy: pallet_proxy,
		Preimage: pallet_preimage,
		Rc2Migrator: pallet_rc2_migrator,
	}
}

impl<C> frame_system::offchain::CreateTransactionBase<C> for Test
where
	RuntimeCall: From<C>,
{
	type Extrinsic = UncheckedExtrinsic;
	type RuntimeCall = RuntimeCall;
}

impl<C> frame_system::offchain::CreateBare<C> for Test
where
	RuntimeCall: From<C>,
{
	fn create_bare(call: Self::RuntimeCall) -> Self::Extrinsic {
		UncheckedExtrinsic::new_bare(call)
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

impl shared::Config for Test {
	type DisabledValidators = ();
}

impl parachains_origin::Config for Test {}

impl configuration::Config for Test {
	type WeightInfo = configuration::TestWeightInfo;
}

parameter_types! {
	pub const ParasUnsignedPriority: TransactionPriority = TransactionPriority::MAX;
}

impl paras::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = paras::TestWeightInfo;
	type UnsignedPriority = ParasUnsignedPriority;
	type QueueFootprinter = ();
	type NextSessionRotation = ();
	type OnNewHead = ();
	type AssignCoretime = ();
	type Fungible = Balances;
	type CooldownRemovalMultiplier = ConstU128<1>;
	type AuthorizeCurrentCodeOrigin = EnsureRoot<AccountId>;
}

impl dmp::Config for Test {}

parameter_types! {
	pub const DefaultChannelSizeAndCapacityWithSystem: (u32, u32) = (4096, 4);
}

impl parachains_hrmp::Config for Test {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type ChannelManager = EnsureRoot<AccountId>;
	type Currency = Balances;
	type DefaultChannelSizeAndCapacityWithSystem = DefaultChannelSizeAndCapacityWithSystem;
	type VersionWrapper = ();
	type WeightInfo = parachains_hrmp::TestWeightInfo;
}

parameter_types! {
	pub const ParaDeposit: u128 = 300;
	pub const DataDepositPerByte: u128 = 1;
}

impl paras_registrar::Config for Test {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type OnSwap = ();
	type ParaDeposit = ParaDeposit;
	type DataDepositPerByte = DataDepositPerByte;
	type WeightInfo = paras_registrar::TestWeightInfo;
}

parameter_types! {
	pub const MultisigDepositBase: u128 = 30;
	pub const MultisigDepositFactor: u128 = 5;
}

impl pallet_multisig::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type Currency = Balances;
	type DepositBase = MultisigDepositBase;
	type DepositFactor = MultisigDepositFactor;
	type MaxSignatories = frame_support::traits::ConstU32<5>;
	type WeightInfo = ();
	type BlockNumberProvider = System;
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

impl TryFrom<ProxyType> for PortableProxyType {
	type Error = ();

	fn try_from(t: ProxyType) -> Result<Self, ()> {
		match t {
			ProxyType::Any => Ok(PortableProxyType::Any),
			ProxyType::NonTransfer => Ok(PortableProxyType::NonTransfer),
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

parameter_types! {
	pub const PreimageBaseDeposit: u128 = 1;
	pub const PreimageByteDeposit: u128 = 1;
	pub const PreimageHoldReason: RuntimeHoldReason =
		RuntimeHoldReason::Preimage(pallet_preimage::HoldReason::Preimage);
}

impl pallet_preimage::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = ();
	type Currency = Balances;
	type ManagerOrigin = EnsureRoot<AccountId>;
	type Consideration = frame_support::traits::fungible::HoldConsideration<
		AccountId,
		Balances,
		PreimageHoldReason,
		frame_support::traits::LinearStoragePrice<PreimageBaseDeposit, PreimageByteDeposit, u128>,
	>;
}

/// The account the mock treats as the Coretime chain's dispatch origin.
pub const CORETIME: AccountId = AccountId32::new([5; 32]);

/// Somebody
pub const ALICE: AccountId = AccountId32::new([1; 32]);

/// The account behind the admin origin.
pub const ADMIN: AccountId = AccountId32::new([2; 32]);

pub const CT_PARA_ID: u32 = 1005;
pub const AH_PARA_ID: u32 = 1000;
pub const WARM_UP: u64 = 4;
pub const COOL_OFF: u64 = 10;
/// Relay-chain block time
pub const BLOCK_TIME_MS: u64 = 6_000;

parameter_types! {
	pub const CtParaId: u32 = CT_PARA_ID;
	pub const AhParaId: u32 = AH_PARA_ID;

	/// Every message the pallet successfully sent, in order.
	pub static SentXcm: Vec<(Location, Xcm<()>)> = vec![];
	/// Makes the router reject everything, to exercise the retry-next-block path.
	pub static SendFails: bool = false;

	/// The mock wall clock, in milliseconds. Advanced by [`run_blocks`].
	pub static MockNow: u64 = BLOCK_TIME_MS;

	/// Working buffer that follows deposits to the Coretime chain.
	pub const CtFreeBuffer: u128 = 100;
	/// Asset Hub's ED: half the relay's in this mock, so the dust-follows-deposit rule has a
	/// window (a teleport of 1..=4 is valid nowhere).
	pub const AhExistentialDeposit: u128 = 5;
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
	type AhParaId = AhParaId;
	type TimeProvider = MockTime;
	type CtOrigin = EnsureSignedBy<CoretimeAccount, AccountId>;
	type AdminOrigin = EnsureSignedBy<AdminAccount, AccountId>;
	type CtFreeBuffer = CtFreeBuffer;
	type AhExistentialDeposit = AhExistentialDeposit;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	SentXcm::set(vec![]);
	SendFails::set(false);
	MockNow::set(BLOCK_TIME_MS);

	let mut storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	// A consistent host configuration; the zeroed default fails the genesis consistency check.
	configuration::GenesisConfig::<Test> {
		config: configuration::HostConfiguration {
			max_code_size: 3 * 1024 * 1024,
			max_head_data_size: 1024 * 1024,
			..Default::default()
		},
	}
	.assimilate_storage(&mut storage)
	.unwrap();
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

/// A pallet (module) account: the kind the migration leaves for the sweep stage.
pub fn pot() -> AccountId {
	PalletId(*b"py/trsry").into_account_truncating()
}

/// The child sovereign account of a para on the relay chain (`para…`).
pub fn child_sov(para: u32) -> AccountId {
	ParaId::from(para).into_account_truncating()
}

pub fn fund(who: &AccountId, amount: u128) {
	let _ = <Balances as Currency<AccountId>>::make_free_balance_be(who, amount);
}

pub fn reserve(who: &AccountId, amount: u128) {
	<Balances as ReservableCurrency<AccountId>>::reserve(who, amount).unwrap();
}

pub fn lock(who: &AccountId, amount: u128) {
	<Balances as LockableCurrency<AccountId>>::set_lock(
		*b"testlock",
		who,
		amount,
		WithdrawReasons::all(),
	);
}

pub fn free(who: &AccountId) -> u128 {
	pallet_balances::Pallet::<Test>::free_balance(who)
}

pub fn reserved(who: &AccountId) -> u128 {
	pallet_balances::Pallet::<Test>::reserved_balance(who)
}

pub fn total_issuance() -> u128 {
	pallet_balances::TotalIssuance::<Test>::get()
}

/// Register a para through the real registrar path (`reserve`): records `ParaDeposit` (= 300)
/// against the manager and reserves it, exactly like mainnet state.
pub fn register_para(id: u32, manager: &AccountId) {
	paras_registrar::NextFreeParaId::<Test>::put(ParaId::from(id));
	Registrar::reserve(RuntimeOrigin::signed(manager.clone())).expect("manager can reserve");
}

/// Insert an HRMP channel with its deposits reserved on the child sovereigns — the state the
/// real channel-open handshake leaves behind (the handshake itself needs live paras + sessions,
/// far beyond unit scope).
pub fn open_channel(sender: u32, recipient: u32, sender_deposit: u128, recipient_deposit: u128) {
	for (para, deposit) in [(sender, sender_deposit), (recipient, recipient_deposit)] {
		let sov = child_sov(para);
		fund(&sov, free(&sov) + deposit + ED);
		reserve(&sov, deposit);
	}
	let id = HrmpChannelId { sender: sender.into(), recipient: recipient.into() };
	parachains_hrmp::HrmpChannels::<Test>::insert(
		&id,
		parachains_hrmp::HrmpChannel {
			max_capacity: 8,
			max_total_size: 4096,
			max_message_size: 1024,
			msg_count: 0,
			total_size: 0,
			mqc_head: None,
			sender_deposit,
			recipient_deposit,
		},
	);
}

/// Insert a pending open-channel request with the sender deposit reserved, mirroring
/// `hrmp_init_open_channel`'s end state.
pub fn open_request(sender: u32, recipient: u32, deposit: u128) {
	let sov = child_sov(sender);
	fund(&sov, free(&sov) + deposit + ED);
	reserve(&sov, deposit);
	let id = HrmpChannelId { sender: sender.into(), recipient: recipient.into() };
	parachains_hrmp::HrmpOpenChannelRequests::<Test>::insert(
		&id,
		parachains_hrmp::HrmpOpenChannelRequest {
			confirmed: false,
			_age: 0,
			sender_deposit: deposit,
			max_message_size: 1024,
			max_capacity: 8,
			max_total_size: 4096,
		},
	);
	parachains_hrmp::HrmpOpenChannelRequestsList::<Test>::mutate(|list| list.push(id));
	parachains_hrmp::HrmpOpenChannelRequestCount::<Test>::mutate(ParaId::from(sender), |c| *c += 1);
}

/// Grant a proxy through the real pallet path; reserves the deposit at this chain's rates.
/// Calling the dispatchable directly does not bump the delegator's nonce, so a never-signed
/// delegator stays at nonce 0 — exactly how pures and multisigs look on chain.
pub fn add_proxy(delegator: &AccountId, delegate: &AccountId, proxy_type: ProxyType) {
	Proxy::add_proxy(RuntimeOrigin::signed(delegator.clone()), delegate.clone(), proxy_type, 0)
		.expect("can add proxy");
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
pub fn force_anomalous_account(who: &AccountId, free: u128, reserved: u128, consumers: u32) {
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
