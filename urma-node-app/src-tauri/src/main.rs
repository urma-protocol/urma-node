#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(
    unused_must_use,
    for_loops_over_fallibles,
    dead_code,
    unused_variables,
    unused_assignments
)]

fn main() -> Result<(), tauri::Error> {
    urma_node_app::run()
}
