// This file is part of Substrate.

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

//! Common traits and types that are useful for describing offences for usage in environments
//! that use staking.

use sp_std::vec::Vec;

use codec::{Decode, Encode};

/// The kind of an offence, is a byte string representing some kind identifier
/// e.g. `b"poc:equivocation"`
pub type Kind = [u8; 16];

/// A trait implemented by an offence report.
///
/// This trait assumes that the offence is legitimate and was validated already.
///
/// Examples of offences include: a BABE equivocation or a GRANDPA unjustified vote.
pub trait Offence<Offender> {
    /// Identifier which is unique for this kind of an offence.
    const ID: Kind;

    /// A type that represents a point in time on an abstract timescale.
    ///
    /// See `Offence::time_slot` for details. The only requirement is that such timescale could be
    /// represented by a single `u128` value.
    type TimeSlot: Clone + codec::Codec + Ord;

    /// The list of all offenders involved in this incident.
    ///
    /// The list has no duplicates, so it is rather a set.
    fn offenders(&self) -> Vec<Offender>;

    /// A point in time when this offence happened.
    ///
    /// This is used for looking up offences that happened at the "same time".
    ///
    /// The timescale is abstract and doesn't have to be the same across different implementations
    /// of this trait. The value doesn't represent absolute timescale though since it is interpreted
    /// along with the `session_index`. Two offences are considered to happen at the same time iff
    /// both `session_index` and `time_slot` are equal.
    ///
    /// As an example, for GRANDPA timescale could be a round number and for BABE it could be a slot
    /// number. Note that for GRANDPA the round number is reset each epoch.
    fn time_slot(&self) -> Self::TimeSlot;
}

/// Errors that may happen on offence reports.
#[derive(PartialEq, sp_runtime::RuntimeDebug)]
pub enum OffenceError {
    /// The report has already been sumbmitted.
    DuplicateReport,

    /// Other error has happened.
    Other(u8),
}

impl sp_runtime::traits::Printable for OffenceError {
    fn print(&self) {
        "OffenceError".print();
        match self {
            Self::DuplicateReport => "DuplicateReport".print(),
            Self::Other(e) => {
                "Other".print();
                e.print();
            }
        }
    }
}

/// A trait for decoupling offence reporters from the actual handling of offence reports.
pub trait ReportOffence<Offender, O: Offence<Offender>> {
    /// Report an `offence` and reward given `reporters`.
    fn report_offence(offence: O) -> Result<(), OffenceError>;

    /// Returns true iff all of the given offenders have been previously reported
    /// at the given time slot. This function is useful to prevent the sending of
    /// duplicate offence reports.
    fn is_known_offence(offenders: &[Offender], time_slot: &O::TimeSlot) -> bool;
}

impl<Offender, O: Offence<Offender>> ReportOffence<Offender, O> for () {
    fn report_offence(_offence: O) -> Result<(), OffenceError> {
        Ok(())
    }

    fn is_known_offence(_offenders: &[Offender], _time_slot: &O::TimeSlot) -> bool {
        true
    }
}

/// A trait to take action on an offence.
///
/// Used to decouple the module that handles offences and
/// the one that should punish for those offences.
pub trait OnOffenceHandler<Offender> {
    /// A handler for an offence of a particular kind.
    ///
    /// Note that this contains a list of all previous offenders
    /// as well. The implementer should cater for a case, where
    /// the same farmers were reported for the same offence
    /// in the past (see `OffenceCount`).
    fn on_offence(offenders: &[OffenceDetails<Offender>]);
}

impl<Offender> OnOffenceHandler<Offender> for () {
    fn on_offence(_offenders: &[OffenceDetails<Offender>]) {}
}

/// A details about an offending authority for a particular kind of offence.
#[derive(Clone, PartialEq, Eq, Encode, Decode, sp_runtime::RuntimeDebug)]
pub struct OffenceDetails<Offender> {
    /// The offending authority id
    pub offender: Offender,
}
