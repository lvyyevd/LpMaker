//! SOL-only six-month research: fixed APR is an assumption, never live account income.
mod data;
mod search;
mod simulation;
mod verification;
pub use data::{Bar, Data, load};
pub use search::{Parameters, configure, run};
pub use simulation::{Outcome, cached_signals, fee_for_bar, simulate};
pub use verification::verify;
const HOUR: u64 = 3_600_000;
