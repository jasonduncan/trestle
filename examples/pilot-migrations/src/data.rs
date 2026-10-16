//! Data steps referenced by migration manifests (`"data": "<name>"`).
//!
//! Each step is `pub fn <name>() -> DataStep` whose `source` is the text of
//! its own module (`include_str!`), so the frozen code is part of the
//! migration digest. Steps use `HistoricalTx` only: stable model IDs and
//! field keys, never types from the application crates.

use trestle_historical::DataStep;

/// Every data step, registered by the name manifests use. None yet.
pub fn steps() -> Vec<DataStep> {
    Vec::new()
}
