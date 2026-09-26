use serde::{
    Deserialize,
    Serialize,
};
pub const DEFAULT_DANMAKU_SERVER_LABEL: &str = "dandanplay";
pub const DEFAULT_DANMAKU_SERVER_URL: &str = "https://api.dandanplay.net";
pub const DEFAULT_DANMAKU_APP_ID: &str = "e9imrhcexn";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DanmakuServer {
    pub name: String,
    pub url: String,
}

impl DanmakuServer {
    pub fn try_new(name: &str, url: &str) -> Result<Self, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Server name cannot be empty.".to_string());
        }

        let url = normalize_danmaku_server_url(url)?;
        Ok(Self {
            name: name.to_string(),
            url,
        })
    }
}

pub fn normalize_danmaku_server_url(url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(url.trim()).map_err(|_| "Invalid server URL.".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("Server URL must use HTTP or HTTPS.".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Server URL cannot contain credentials.".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("Server URL cannot contain a query or fragment.".to_string());
    }

    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

pub fn validate_danmaku_credentials(app_id: &str, app_secret: &str) -> Result<(), String> {
    let app_id = app_id.trim();
    let app_secret = app_secret.trim();
    if app_secret.is_empty() && (app_id.is_empty() || app_id == DEFAULT_DANMAKU_APP_ID) {
        return Ok(());
    }
    if app_id.is_empty() || app_secret.is_empty() {
        return Err("Custom credentials require both App ID and App Secret.".to_string());
    }
    if reqwest::header::HeaderValue::from_str(app_id).is_err() {
        return Err("App ID contains invalid header characters.".to_string());
    }

    Ok(())
}

pub fn uses_builtin_danmaku_credentials(app_id: &str, app_secret: &str) -> bool {
    let app_id = app_id.trim();
    app_secret.trim().is_empty() && (app_id.is_empty() || app_id == DEFAULT_DANMAKU_APP_ID)
}

pub fn danmaku_combo_to_server_index(selected: u32) -> i32 {
    selected as i32 - 1
}

pub fn danmaku_server_to_combo_index(server_index: i32) -> u32 {
    server_index.saturating_add(1) as u32
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
        assert!(validate_danmaku_credentials("", "").is_ok());
        assert!(validate_danmaku_credentials(DEFAULT_DANMAKU_APP_ID, "").is_ok());
        assert!(validate_danmaku_credentials("custom-app", "custom-secret").is_ok());
        assert!(validate_danmaku_credentials("custom-app", "").is_err());
        assert!(validate_danmaku_credentials("", "custom-secret").is_err());
        assert!(validate_danmaku_credentials("bad\napp", "custom-secret").is_err());
    }

    #[test]
    fn validates_and_normalizes_danmaku_servers() {
        let server =
            DanmakuServer::try_new(" Local ", "https://example.com/api/").expect("valid server");
        assert_eq!(server.name, "Local");
        assert_eq!(server.url, "https://example.com/api");
        assert!(DanmakuServer::try_new("", "https://example.com").is_err());
        assert!(DanmakuServer::try_new("Local", "file:///tmp/api").is_err());
        assert!(DanmakuServer::try_new("Local", "https://user@example.com").is_err());
        assert!(DanmakuServer::try_new("Local", "https://example.com?token=value").is_err());
    }
}
