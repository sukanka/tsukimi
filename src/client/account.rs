use serde::{
    Deserialize,
    Serialize,
};

use crate::ui::provider::descriptor::VecSerialize;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Route {
    pub name: String,
    pub server: String,
    pub port: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]

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

#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
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
        match self.active_route() {
            Some(route) => (&route.server, &route.port, route.path.as_deref()),
            None => (&self.server, &self.port, self.path.as_deref()),
        }
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
