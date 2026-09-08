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

//! HRMP stage: **copies** the `hrmp` pallet's channel records and pending open-channel requests to
//! the Coretime chain in portable format, and zeroes the deposits on the records left behind.
//!
//! Copied, not drained, and that distinction is load-bearing. The relay chain routes every HRMP
//! message through `HrmpChannels` — `check_outbound_hrmp` refuses a candidate whose channel it
//! cannot find — and it completes an open handshake at a *session boundary*, from
//! `HrmpOpenChannelRequests`. Removing either map stops parachains talking to each other and
//! destroys handshakes that have not yet been promoted, including the control-plane channels the
//! registrar stage asks for on its way through. The ingress/egress indexes are maintained
//! incrementally against `HrmpChannels` and are never rebuilt, so a drain also leaves them
//! permanently orphaned — which the relay chain's own HRMP try-state asserts against.
//!
//! What moves is the **money**, and only the money:
//! - The channel and request deposits travel via the accounts stage: they arrive as holds on the
//!   paras' *sibling sovereign* accounts and are re-attributed by the receiving side as each record
//!   lands. Coretime is then the sole authority on them.
//! - So the deposit fields on the records retained here are zeroed. Closing a channel refunds
//!   `sender_deposit`/`recipient_deposit` from the paras' sovereign accounts, which the accounts
//!   stage has emptied; a non-zero figure left behind is a refund against money that is no longer
//!   there. Zero is also how the relay chain is driven afterwards — `HrmpRegistry` opens every
//!   channel with a zero deposit override.
//!
//! The dynamic message state (`msg_count`, `total_size`, `mqc_head`) stays where it is read and is
//! not part of the wire format.

use crate::*;
use runtime_parachains::hrmp::{
	HrmpChannels, HrmpOpenChannelRequests, HrmpOpenChannelRequestsList,
};

pub struct HrmpMigrator<T>(PhantomData<T>);

impl<T: Config> HrmpMigrator<T> {
	/// Copy every pending open-channel request to the Coretime chain, which takes over accounting
	/// for their deposits, and zero the deposit on the copy left here.
	///
	/// The requests themselves stay: the relay chain promotes them to channels at its next session
	/// boundary, and it is the only thing that can. `HrmpOpenChannelRequestCount` and
	/// `HrmpAcceptedChannelRequestCount` stay with them, because they bound how many requests a
	/// para may have outstanding and the relay chain still enforces that.
	///
	/// One-shot, called by `HrmpInit`; the request count is small (dozens). The caller wraps
	/// this in a storage transaction so a failed send rolls everything back for a retry.
	pub fn copy_open_requests() -> Result<(), Error<T>> {
		let mut batch = Vec::new();
		for id in HrmpOpenChannelRequestsList::<T>::get() {
			HrmpOpenChannelRequests::<T>::mutate(&id, |maybe_request| {
				if let Some(request) = maybe_request {
					batch.push(migrator_types::PortableHrmpRequest {
						sender: id.sender.into(),
						recipient: id.recipient.into(),
						confirmed: request.confirmed,
						sender_deposit: request.sender_deposit,
						max_message_size: request.max_message_size,
						max_capacity: request.max_capacity,
						max_total_size: request.max_total_size,
					});
					request.sender_deposit = 0;
				}
			});
		}

		let count = batch.len() as u32;
		while !batch.is_empty() {
			let rest = batch.split_off(batch.len().min(MAX_RECORDS_PER_XCM as usize));
			Pallet::<T>::send_hrmp_requests(core::mem::replace(&mut batch, rest))?;
		}
		Pallet::<T>::deposit_event(Event::HrmpRequestsSent { count });
		Ok(())
	}

	/// Copy HRMP channel records until the per-block limit is reached, zeroing the deposits on the
	/// records left behind.
	///
	/// Returns the cursor to continue from on the next block, or `None` once the map is
	/// exhausted. The caller wraps this in a storage transaction; an `Err` rolls back the whole
	/// block's writes.
	pub fn migrate_many(
		last_key: Option<HrmpChannelId>,
	) -> Result<Option<HrmpChannelId>, Error<T>> {
		let iter = match &last_key {
			Some(last_key) => HrmpChannels::<T>::iter_from_key(last_key),
			None => HrmpChannels::<T>::iter(),
		};

		// The cursor comes from `iter_from_key`, which resumes strictly after the last key seen,
		// so progress does not depend on the records being removed.
		Pallet::<T>::drain_records(
			iter,
			|channel_id, mut channel| {
				let portable = PortableHrmpChannel {
					sender: channel_id.sender.into(),
					recipient: channel_id.recipient.into(),
					max_capacity: channel.max_capacity,
					max_total_size: channel.max_total_size,
					max_message_size: channel.max_message_size,
					sender_deposit: channel.sender_deposit,
					recipient_deposit: channel.recipient_deposit,
				};

				// The record stays — the relay chain routes through it. Only the deposits leave.
				channel.sender_deposit = 0;
				channel.recipient_deposit = 0;
				HrmpChannels::<T>::insert(channel_id, channel);

				Ok(Some(portable))
			},
			Pallet::<T>::send_hrmp,
		)
	}
}
