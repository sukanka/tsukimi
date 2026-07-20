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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DanmakuServer {
    pub name: String,
    pub url: String,
}

pub fn danmaku_combo_to_server_index(selected: u32) -> i32 {
    selected as i32 - 1
}

pub fn danmaku_server_to_combo_index(server_index: i32) -> u32 {
    server_index.saturating_add(1) as u32
}

pub fn apply_danmaku_active_server(active: i32, servers: &[DanmakuServer]) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let url = usize::try_from(active)
            .ok()
            .and_then(|index| servers.get(index))
            .map_or("", |server| server.url.as_str());
        dandanapi::set_base_uri(url).map_err(|error| error.to_string())
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (active, servers);
        Ok(())
    }
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

        let result = if is_default_danmaku_credentials(appid, appsecret) {
            let generator = dandanapi::SecretGenerator::new(
                include_bytes!("../../secret/secret").to_vec(),
                SECRET_KEY.to_string(),
            );
            dandanapi::DanDanClient::init(X_APPID.to_string(), generator)
        } else {
            dandanapi::DanDanClient::init(appid.to_string(), appsecret.to_string())
        };

        result.map_err(|error| error.to_string())
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (appid, appsecret);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_combo_indices_to_server_indices() {
        assert_eq!(danmaku_combo_to_server_index(0), -1);
        assert_eq!(danmaku_combo_to_server_index(2), 1);
        assert_eq!(danmaku_server_to_combo_index(-1), 0);
        assert_eq!(danmaku_server_to_combo_index(1), 2);
    }

    #[test]
    fn validates_custom_credentials_as_a_pair() {
        assert!(is_default_danmaku_credentials("", ""));
        assert!(is_default_danmaku_credentials(default_danmaku_appid(), ""));
        assert!(has_complete_danmaku_credentials("app", "secret"));
        assert!(!has_complete_danmaku_credentials("app", ""));
    }
}
