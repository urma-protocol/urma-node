#![deny(
    unused_must_use,
    for_loops_over_fallibles,
    dead_code,
    unused_variables,
    unused_assignments
)]

mod app;
mod config;
mod error;
mod node;

pub use app::run;
