pub mod direct;
mod intent;
pub(crate) mod manga_transfer;
pub mod monitor;
pub mod pipeline;

pub use intent::{Intent, IntentError, Intents};

#[cfg(test)]
#[path = "intent_test.rs"]
mod tests;
