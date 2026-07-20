use serde::{
    Deserialize,
    Serialize,
};

use crate::ui::provider::descriptor::VecSerialize;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Route {
    pub name: String,
    pub server: String,
    pub port: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]

pub enum ServerType {
    #[default]
    Emby = 0,
    Jellyfin = 1,
}

impl ServerType {
    pub fn index(self) -> u32 {
        self as u32
    }

    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Jellyfin,
            _ => Self::Emby,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Account {
    pub servername: String,
    pub server: String,
    pub username: String,
    pub password: String,
    pub port: String,
    pub user_id: String,
    pub access_token: String,
    pub server_type: Option<ServerType>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub route_name: Option<String>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub active_route: Option<usize>,
}

impl Account {
    pub fn active_route(&self) -> Option<&Route> {
        self.active_route.and_then(|index| self.routes.get(index))
    }

    pub fn active_endpoint(&self) -> (&str, &str, Option<&str>) {
        self.active_route()
            .map(|route| {
                (
                    route.server.as_str(),
                    route.port.as_str(),
                    route.path.as_deref(),
                )
            })
            .unwrap_or((
                self.server.as_str(),
                self.port.as_str(),
                self.path.as_deref(),
            ))
    }

    pub fn main_route_name(&self) -> &str {
        self.route_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.servername)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub struct Accounts {
    pub accounts: Vec<Account>,
}

impl VecSerialize<Account> for Vec<Account> {
    fn to_string(&self) -> String {
        serde_json::to_string(&self).expect("Failed to serialize Vec<Descriptor>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            servername: "Home".into(),
            server: "https://home.example".into(),
            port: "443".into(),
            path: Some("jellyfin".into()),
            routes: vec![Route {
                name: "Remote".into(),
                server: "https://remote.example".into(),
                port: "8443".into(),
                path: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn active_endpoint_uses_selected_route() {
        let mut account = account();
        account.active_route = Some(0);

        assert_eq!(
            account.active_endpoint(),
            ("https://remote.example", "8443", None)
        );
    }

    #[test]
    fn invalid_active_route_falls_back_to_main_endpoint() {
        let mut account = account();
        account.active_route = Some(99);

        assert_eq!(
            account.active_endpoint(),
            ("https://home.example", "443", Some("jellyfin"))
        );
    }

    #[test]
    fn legacy_account_json_uses_route_defaults() {
        let account: Account = serde_json::from_str(
            r#"{
                "servername":"Home",
                "server":"https://home.example",
                "username":"user",
                "password":"",
                "port":"443",
                "user_id":"id",
                "access_token":"token",
                "server_type":"Jellyfin"
            }"#,
        )
        .unwrap();

        assert!(account.path.is_none());
        assert!(account.route_name.is_none());
        assert!(account.routes.is_empty());
        assert!(account.active_route.is_none());
    }
}
