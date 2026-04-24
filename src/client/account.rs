use serde::{
    Deserialize,
    Serialize,
};

use crate::ui::provider::descriptor::VecSerialize;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct Route {
    pub name: String,
    pub server: String,
    pub port: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Account {
    pub servername: String,
    pub server: String,
    pub username: String,
    pub password: String,
    pub port: String,
    pub user_id: String,
    pub access_token: String,
    pub server_type: Option<String>,
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
    pub fn active_server(&self) -> &str {
        match self.active_route.and_then(|i| self.routes.get(i)) {
            Some(r) => &r.server,
            None => &self.server,
        }
    }

    pub fn active_port(&self) -> &str {
        match self.active_route.and_then(|i| self.routes.get(i)) {
            Some(r) => &r.port,
            None => &self.port,
        }
    }

    pub fn active_path(&self) -> Option<&str> {
        match self.active_route.and_then(|i| self.routes.get(i)) {
            Some(r) => r.path.as_deref(),
            None => self.path.as_deref(),
        }
    }

    pub fn main_route_name(&self) -> &str {
        self.route_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
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
