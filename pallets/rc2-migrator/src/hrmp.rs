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

//! HRMP stage: drains the `hrmp` pallet's channel records and pending open-channel requests from
//! the relay chain and sends them to the Coretime chain in portable format.
//!
//! Records only, deliberately:
//! - The channel and request deposits travel via the accounts stage: they arrive as holds on the
//!   paras' *sibling sovereign* accounts and are re-attributed by the receiving side as each record
//!   lands.
//! - The dynamic message state (`msg_count`, `total_size`, `mqc_head`) is not migrated; channels
//!   are expected to be drained of messages before the migration runs.
//! - The ingress/egress indexes are not migrated.

use crate::*;
use runtime_parachains::hrmp::HrmpChannels;

pub struct HrmpMigrator<T>(PhantomData<T>);

impl<T: Config> HrmpMigrator<T> {
	/// Drain all pending open-channel requests and send them to the Coretime chain, where the
	/// future HRMP system decides whether the handshakes can finish.
	///
	/// One-shot, called by `HrmpInit`; the request count is small (dozens). The caller wraps
	/// this in a storage transaction so a failed send rolls everything back for a retry.
	pub fn drain_open_requests() -> Result<(), Error<T>> {
		use runtime_parachains::hrmp::{
			HrmpAcceptedChannelRequestCount, HrmpOpenChannelRequestCount, HrmpOpenChannelRequests,
			HrmpOpenChannelRequestsList,
		};

		let mut batch = Vec::new();
		for id in HrmpOpenChannelRequestsList::<T>::take() {
			if let Some(request) = HrmpOpenChannelRequests::<T>::take(&id) {
				batch.push(migrator_types::PortableHrmpRequest {
					sender: id.sender.into(),
					recipient: id.recipient.into(),
					confirmed: request.confirmed,
					sender_deposit: request.sender_deposit,
					max_message_size: request.max_message_size,
					max_capacity: request.max_capacity,
					max_total_size: request.max_total_size,
				});
			}
		}
		let _ = HrmpOpenChannelRequestCount::<T>::clear(u32::MAX, None);
		let _ = HrmpAcceptedChannelRequestCount::<T>::clear(u32::MAX, None);

		let count = batch.len() as u32;
		for chunk in batch.chunks(MAX_RECORDS_PER_XCM as usize) {
			Pallet::<T>::send_hrmp_requests(chunk.to_vec())?;
		}
		Pallet::<T>::deposit_event(Event::HrmpRequestsSent { count });
		Ok(())
	}

	/// Drain HRMP channel records until the per-block limit is reached.
	///
	/// Returns the cursor to continue from on the next block, or `None` once the map is
	/// exhausted. The caller wraps this in a storage transaction; an `Err` rolls back the whole
	/// block's removals.
	pub fn migrate_many(
		last_key: Option<HrmpChannelId>,
	) -> Result<Option<HrmpChannelId>, Error<T>> {
		let iter = match &last_key {
			Some(last_key) =>
				HrmpChannels::<T>::iter_from(HrmpChannels::<T>::hashed_key_for(last_key)),
			None => HrmpChannels::<T>::iter(),
		};

		Pallet::<T>::drain_records(
			iter,
			|channel_id, channel| {
				HrmpChannels::<T>::remove(channel_id);
				PortableHrmpChannel {
					sender: channel_id.sender.into(),
					recipient: channel_id.recipient.into(),
					max_capacity: channel.max_capacity,
					max_total_size: channel.max_total_size,
					max_message_size: channel.max_message_size,
					sender_deposit: channel.sender_deposit,
					recipient_deposit: channel.recipient_deposit,
				}
			},
			Pallet::<T>::send_hrmp,
		)
	}
}
