//! Rust evaluator for the Nix expression language, reached from cppnix
//! through the C ABI in `capi`. Pipeline: rnix CST -> `compile` -> `ir`
//! module -> `vm` -> `print`. See ENG-12068.

/// Every Rust allocation in this crate goes through mimalloc, in whichever
/// process links us: the profile on ENG-13148 spends ~17% of a NixOS
/// toplevel eval in glibc malloc/free, and swapping the allocator alone
/// was measured at ~13% cpu (A/B/A/B, identical output; numbers beside the
/// dependency in Cargo.toml). What this does not cover is the C++ half of
/// the `nix` binary, which keeps its own allocator; the LD_PRELOAD
/// experiment covered both and scored the same 13%, so the Rust side is
/// where the traffic is.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod abi_check;
pub mod builtin_catalogue;
pub mod builtins;
pub mod capi;
mod closure_diff;
pub mod compile;
pub mod deepwalk;
pub mod drv;
mod drv_name;
pub mod drvpath;
pub mod drvstrict;
pub mod eval;
pub mod flake_doc;
mod flake_show;
mod search;
mod terminal;
mod suggestions;
pub mod flake_check;
pub mod host;
mod imported_drv;
pub(crate) mod import_cache;
pub mod ir;
pub mod lock_graph;
pub mod modcache;
pub mod nixhash;
pub mod perf;
pub mod primops_host;
pub mod primops_pure;
pub mod print;
pub mod purity;
pub mod readset;
pub mod refusal;
pub mod session;
pub mod store;
mod store_batch;
pub mod storepath;
pub mod task;
pub mod value2;
pub mod vm;
/// `builtins.wasm`: WebAssembly guests over the `env` host interface.
pub mod wasm;

