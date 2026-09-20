//! SOL-only six-month research: fixed APR is an assumption, never live account income.
mod data;
mod search;
mod simulation;
pub mod training;
mod verification;
pub use data::{Bar, Data, load};
pub use search::{Parameters, configure, run};
pub use simulation::{
    AccountModel, Outcome, cached_signals, fee_for_bar, simulate, simulate_account,
};
pub use verification::{verify, verify_account};
const HOUR: u64 = 3_600_000;
