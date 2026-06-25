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

pub const DEFAULT_DANMAKU_SERVER_LABEL: &str = "dandanplay";

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

pub fn default_danmaku_appid() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        X_APPID
    }

    #[cfg(not(target_os = "linux"))]
    {
        ""
    }
}

pub fn is_default_danmaku_credentials(appid: &str, appsecret: &str) -> bool {
    let appid = appid.trim();
    let appsecret = appsecret.trim();
    appsecret.is_empty() && (appid.is_empty() || appid == default_danmaku_appid())
}

pub fn has_complete_danmaku_credentials(appid: &str, appsecret: &str) -> bool {
    !appid.trim().is_empty() && !appsecret.trim().is_empty()
}

pub fn init_danmaku_client_without_credentials() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        dandanapi::DanDanClient::init(String::new(), String::new())
            .map_err(|error| error.to_string())
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
}

pub fn init_danmaku_client_credentials(appid: &str, appsecret: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let appid = appid.trim();
        let appsecret = appsecret.trim();
        if !is_default_danmaku_credentials(appid, appsecret)
            && !has_complete_danmaku_credentials(appid, appsecret)
        {
            return Err("Please fill App Secret when using a custom App ID.".to_string());
        }

        let init_result = if is_default_danmaku_credentials(appid, appsecret) {
            let generator = dandanapi::SecretGenerator::new(
                include_bytes!("../../secret/secret").to_vec(),
                SECRETE_KEY.to_string(),
            );
            dandanapi::DanDanClient::init(X_APPID.to_string(), generator)
        } else {
            dandanapi::DanDanClient::init(appid.to_string(), appsecret.to_string())
        };

        init_result.map_err(|error| error.to_string())
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (appid, appsecret);
        Ok(())
    }
}
