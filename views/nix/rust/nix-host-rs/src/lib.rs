//! Evaluator-independent store, fetcher and build policies.
//!
//! Every exported handle and buffer is released through its owning domain ABI.
//! This library never links the evaluator or calls C++ through global symbols.

mod abi_check;
mod build_scheduler;
mod fetch_registry;
mod store_stream;
