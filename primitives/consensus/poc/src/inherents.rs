// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
// Copyright (C) 2021 Subspace Labs, Inc.
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

//! Inherents for Proof-of-Capacity (PoC) consensus

use sp_inherents::{Error, InherentData, InherentIdentifier};
#[cfg(feature = "std")]
use sp_inherents::{InherentDataProviders, ProvideInherentData};
#[cfg(feature = "std")]
use sp_timestamp::TimestampInherentData;

#[cfg(feature = "std")]
use codec::Decode;
use sp_std::result::Result;

/// The PoC inherent identifier.
pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"poc0slot";

/// The type of the PoC inherent.
pub type InherentType = sp_consensus_slots::Slot;
/// Auxiliary trait to extract PoC inherent data.
pub trait PoCInherentData {
	/// Get PoC inherent data.
	fn poc_inherent_data(&self) -> Result<InherentType, Error>;
	/// Replace PoC inherent data.
	fn poc_replace_inherent_data(&mut self, new: InherentType);
}

impl PoCInherentData for InherentData {
	fn poc_inherent_data(&self) -> Result<InherentType, Error> {
		self.get_data(&INHERENT_IDENTIFIER)
			.and_then(|r| r.ok_or_else(|| "PoC inherent data not found".into()))
	}

	fn poc_replace_inherent_data(&mut self, new: InherentType) {
		self.replace_data(INHERENT_IDENTIFIER, &new);
	}
}

/// Provides the slot duration inherent data for PoC.
// TODO: Remove in the future. https://github.com/paritytech/substrate/issues/8029
#[cfg(feature = "std")]
pub struct InherentDataProvider {
	slot_duration: std::time::Duration,
}

#[cfg(feature = "std")]
impl InherentDataProvider {
	/// Constructs `Self`
	pub fn new(slot_duration: std::time::Duration) -> Self {
		Self { slot_duration }
	}
}

#[cfg(feature = "std")]
impl ProvideInherentData for InherentDataProvider {
	fn on_register(&self, providers: &InherentDataProviders) -> Result<(), Error> {
		if !providers.has_provider(&sp_timestamp::INHERENT_IDENTIFIER) {
			// Add the timestamp inherent data provider, as we require it.
			providers.register_provider(sp_timestamp::InherentDataProvider)
		} else {
			Ok(())
		}
	}

	fn inherent_identifier(&self) -> &'static InherentIdentifier {
		&INHERENT_IDENTIFIER
	}

	fn provide_inherent_data(&self, inherent_data: &mut InherentData) -> Result<(), Error> {
		let timestamp = inherent_data.timestamp_inherent_data()?;
		let slot = *timestamp / self.slot_duration.as_millis() as u64;
		inherent_data.put_data(INHERENT_IDENTIFIER, &slot)
	}

	fn error_to_string(&self, error: &[u8]) -> Option<String> {
		Error::decode(&mut &error[..]).map(|e| e.into_string()).ok()
	}
}