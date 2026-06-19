use serde::{
    Deserialize,
    Serialize,
};

pub mod account;
#[cfg(target_os = "linux")]
pub mod dandan;
pub mod error;
pub mod jellyfin_client;
pub mod proxy;
pub mod runtime;
pub mod structs;

pub use account::{
    Account,
    Route,
};
#[cfg(target_os = "linux")]
pub use dandan::*;
pub use proxy::ReqClient;

pub const DEFAULT_DANMAKU_SERVER_LABEL: &str = "Default (api.dandanplay.net)";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DanmakuServer {
    pub name: String,
    pub url: String,
}

pub fn danmaku_combo_to_server_index(selected: u32) -> i32 {
    selected as i32 - 1
}

pub fn danmaku_server_to_combo_index(server_index: i32) -> u32 {
    (server_index + 1) as u32
}

pub fn apply_danmaku_active_server(active: i32, servers: &[DanmakuServer]) {
    #[cfg(target_os = "linux")]
    {
        if active >= 0 && (active as usize) < servers.len() {
            let _ = dandanapi::set_base_uri(&servers[active as usize].url);
        } else {
            let _ = dandanapi::set_base_uri("");
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = (active, servers);
}
