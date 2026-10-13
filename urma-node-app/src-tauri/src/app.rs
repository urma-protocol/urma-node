use crate::node;

pub fn run() -> Result<(), tauri::Error> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            node::node_status,
            node::prune_cache,
            node::overlay_omissions,
            node::overlay_book
        ])
        .run(tauri::generate_context!())
}
