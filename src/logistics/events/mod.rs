//! Durable publish/subscribe on MySQL: a request records an event in the same
//! transaction as its data, and consumers do the follow-up work in the
//! background with retries. See `docs/async-events.md`.

pub mod queue;
pub mod worker;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod flow_tests;
