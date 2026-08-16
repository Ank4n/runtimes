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

//! Portable ("chain-agnostic") wire types of the AHM v2 migration, plus the few helpers whose
//! behavior both sides of the wire must agree on.
//!
//! These are the payloads exchanged between the relay-chain sender (`pallet-rc2-migrator`) and
//! the receiving chains' migrator pallets. They live in their own crate so that no runtime has
//! to depend on another chain's pallets just to speak the wire format: the sending side encodes
//! from these types, each receiving runtime declares what it can represent via ordinary
//! `From`/`TryFrom` impls on them.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	storage::{transactional::with_transaction_opaque_err, TransactionOutcome},
	traits::ConstU32,
	BoundedVec,
};
use polkadot_parachain_primitives::primitives::{Id as ParaId, Sibling};
use scale_info::TypeInfo;
use sp_runtime::traits::AccountIdConversion;

/// Run `f` inside a storage transaction: `Ok` commits, `Err` rolls every write back.
///
/// This is the rollback primitive of the whole migration pipeline — per-item and per-block
/// isolation on both sides use it, so a failure can never leave state half-written.
pub fn with_rollback<R, E>(f: impl FnOnce() -> Result<R, E>) -> Result<R, E> {
	with_transaction_opaque_err(|| match f() {
		Ok(r) => TransactionOutcome::Commit(Ok(r)),
		Err(e) => TransactionOutcome::Rollback(Err(e)),
	})
	.expect("Layer limit is never reached with per-block nesting; qed")
}

/// The sibling-sovereign account of a para: where a (child) para sovereign's balances continue on
/// a parachain.
///
/// Part of the wire contract: the relay chain sends deposits to this account and the receiving
/// chain looks for them on it, so both sides must derive it identically.
pub fn sibling_account<AccountId>(para_id: u32) -> AccountId
where
	Sibling: AccountIdConversion<AccountId>,
{
	Sibling::from(ParaId::from(para_id)).into_account_truncating()
}

/// Account balance payload in portable format.
///
/// The relay chain withdraws an account into this shape and the receiving chain integrates it
/// through its regular fungible APIs, so refcounts and events are indistinguishable from locally
/// created state.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableAccount<AccountId, Balance> {
	/// The account address. Sent verbatim; no account-id translation happens for regular
	/// accounts.
	pub who: AccountId,
	/// Balance that stays liquid on the receiving chain.
	pub free: Balance,
	/// Balance that was not liquid on the relay chain; re-established as holds on the receiving
	/// chain, one per entry, translated via `From<PortableHoldReason>`.
	pub holds: BoundedVec<PortableHold<Balance>, ConstU32<5>>,
}

/// One non-liquid part of a migrated account's balance.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableHold<Balance> {
	pub reason: PortableHoldReason,
	pub amount: Balance,
}

/// Chain-agnostic identity of balance that was not liquid on the relay chain.
///
/// This enum is the wire-level contract for hold translation: the relay-chain migrator classifies
/// every non-liquid part of an account into one of these variants, and each receiving runtime
/// declares what the variant becomes locally by implementing `From<PortableHoldReason>` for its
/// `RuntimeHoldReason`. The mapping is therefore an explicit `match` per runtime, with no
/// pallet-index coupling on the wire.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum PortableHoldReason {
	/// Reserved on the relay chain without a named reason, via the old `Currency` API — how all
	/// relay-chain deposits (`paras_registrar`, `hrmp`, `proxy`) are placed. Attribution to the
	/// pallet owning the deposit happens when that pallet's own state migrates.
	#[codec(index = 0)]
	UnnamedReserve,
}

/// Registrar record (`paras_registrar::ParaInfo`) in portable format.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableParaInfo<AccountId, Balance> {
	pub para_id: u32,
	/// The account that placed the registration deposit and manages the para.
	pub manager: AccountId,
	/// The deposit as recorded by the registrar. Reconciled against the balance that actually
	/// arrived held during the accounts stage; never trusted on its own.
	pub deposit: Balance,
	/// Whether the para is locked from manager control.
	pub locked: Option<bool>,
}

/// HRMP channel record in portable format.
///
/// Records only: the dynamic message state (`msg_count`, `total_size`, `mqc_head`) is
/// deliberately not migrated.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableHrmpChannel<Balance> {
	pub sender: u32,
	pub recipient: u32,
	pub max_capacity: u32,
	pub max_total_size: u32,
	pub max_message_size: u32,
	pub sender_deposit: Balance,
	pub recipient_deposit: Balance,
}

/// A pending HRMP open-channel request in portable format: para `sender` asked to open a
/// channel to `recipient` and reserved `sender_deposit`; the handshake finishes (or not) on the
/// destination chain under its future HRMP system.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableHrmpRequest<Balance> {
	pub sender: u32,
	pub recipient: u32,
	/// Whether the recipient had already accepted.
	pub confirmed: bool,
	pub sender_deposit: Balance,
	pub max_message_size: u32,
	pub max_capacity: u32,
	pub max_total_size: u32,
}

/// Relay-chain proxy permission in portable format.
///
/// Deliberately carries ONLY the permissions the destination represents: the relay side filters
/// before sending, so untranslatable proxy types (Staking, Governance, …) never travel — their
/// definitions stay on the relay chain.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum PortableProxyType {
	#[codec(index = 0)]
	Any,
	#[codec(index = 1)]
	NonTransfer,
	#[codec(index = 2)]
	CancelProxy,
	#[codec(index = 3)]
	ParaRegistration,
}

/// One proxy delegation of a migrated delegator. `delay` is in relay-chain (6s) blocks; the
/// receiving side converts to its own block time.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableProxyDelegate<AccountId> {
	pub delegate: AccountId,
	pub proxy_type: PortableProxyType,
	pub delay: u32,
}

/// A migrated delegator with its (translatable) proxy delegations.
///
/// The original relay-chain deposit does not travel — it is refunded by the accounts stage; the
/// entry is backed at the destination's own deposit rates from the delegator's local balance
/// instead, so keyless (pure) delegators keep control there without anyone having to sign
/// anything.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PortableProxy<AccountId> {
	pub delegator: AccountId,
	pub delegates: BoundedVec<PortableProxyDelegate<AccountId>, ConstU32<32>>,
}
