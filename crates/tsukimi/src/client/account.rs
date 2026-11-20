use anyhow::{
    Context,
    Result,
    anyhow,
    ensure,
};
use serde::{
    Deserialize,
    Serialize,
};
use url::Url;

use crate::ui::provider::descriptor::VecSerialize;

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

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Route {
    pub name: String,
    pub server: String,
    pub port: String,
    #[serde(default)]
    pub path: Option<String>,
}

impl Route {
    pub fn from_url(name: String, address: &str) -> Result<Self> {
        let url = Url::parse(address)?;
        let scheme = url.scheme();
        ensure!(matches!(scheme, "http" | "https"), "Invalid server address");
        let host = url.host_str().context("Invalid server address")?;
        let port = url.port_or_known_default().context("Invalid server port")?;
        let path = url.path().trim_matches('/');
        Ok(Self {
            name,
            server: format!("{scheme}://{host}"),
            port: port.to_string(),
            path: (!path.is_empty()).then(|| path.to_owned()),
        })
    }

    pub fn url(&self) -> Result<Url> {
        build_url(&self.server, &self.port, self.path.as_deref())
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
    pub fn main_route_name(&self) -> &str {
        self.route_name.as_deref().unwrap_or(&self.servername)
    }

    pub fn main_url(&self) -> Result<Url> {
        build_url(&self.server, &self.port, self.path.as_deref())
    }

    pub fn url(&self) -> Result<Url> {
        self.active_route
            .and_then(|index| self.routes.get(index))
            .map_or_else(|| self.main_url(), Route::url)
    }
}

fn build_url(url_str: &str, port: &str, path: Option<&str>) -> Result<Url> {
    let mut url = Url::parse(url_str)?;
    let port = port.parse::<u16>().context("Invalid server port")?;
    url.set_port(Some(port))
        .map_err(|_| anyhow!("Failed to set port"))?;
    let prefix = path.unwrap_or_else(|| url.path()).trim().trim_matches('/');
    let path = if prefix.is_empty() {
        "/".to_owned()
    } else {
        format!("/{prefix}/")
    };
    url.set_path(&path);
    Ok(url)
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
