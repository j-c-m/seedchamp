//! Application services: add/import orchestration, watch dirs, headless serve entry.
//!
//! TUI and CLI are interfaces; this module owns non-protocol orchestration that
//! talks to catalog + session without holding wire state.

mod add;
mod leech_cache;
mod run;
mod seed;
mod watch;

pub use add::{add_torrent, add_torrent_bytes, load_torrent_bytes, AddOptions, AddReport};
pub use leech_cache::{
    catalog_finish_handoff, choose_placement, copy_payload_to_home, free_space_bytes,
    leech_cache_enabled, remove_leech_cache_tree, wanted_bytes_from_layout,
    wanted_bytes_from_metainfo, Placement,
};
pub use run::serve_main;
pub use seed::{
    default_ltep_client, generate_peer_id, generate_peer_id_with_prefix, pkg_version_major,
    resolve_ltep_client, resolve_peer_id_prefix, run_serve_loop, SeedHandle,
    DEFAULT_PEER_ID_PREFIX, PKG_VERSION,
};
pub use watch::{
    date_stamp, expand_dl_path_template, poll_watch_once, resolve_dl_path, sanitize_path_component,
    spawn_watcher, DlPathContext, WatchCallback, WatchHandle, WatchLoadEvent,
};
