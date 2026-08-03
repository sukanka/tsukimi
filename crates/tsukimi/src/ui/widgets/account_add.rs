use adw::prelude::{
    ActionRowExt,
    AdwDialogExt,
};
use gettextrs::gettext;
use glib::Object;
use gtk::{
    glib,
    prelude::*,
    subclass::prelude::*,
    template_callbacks,
};
use imp::ActionType;

use super::utils::GlobalToast;
use crate::{
    client::{
        Account,
        Route,
        account::ServerType,
        error::UserFacingError,
        jellyfin_client::JELLYFIN_CLIENT,
    },
    ui::models::SETTINGS,
    utils::spawn_tokio,
};
pub mod imp {

    use std::cell::{
        Cell,
        RefCell,
    };

    use adw::subclass::dialog::AdwDialogImpl;
    use glib::subclass::InitializingObject;
    use gtk::{
        CompositeTemplate,
        glib,
        prelude::*,
        subclass::prelude::*,
    };

    use crate::client::{
        Account,
        Route,
    };

    #[derive(Default, Hash, Eq, PartialEq, Clone, Copy, glib::Enum, Debug)]
    #[repr(u32)]
    #[enum_type(name = "ActionType")]
    pub enum ActionType {
        Edit,
        #[default]
        Add,
    }

    // Object holding the state
    #[derive(CompositeTemplate, Default, glib::Properties)]
    #[template(resource = "/moe/tsuna/tsukimi/ui/account.ui")]
    #[properties(wrapper_type = super::AccountWindow)]
    pub struct AccountWindow {
        #[template_child]
        pub servername_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub server_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub username_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub password_entry: TemplateChild<adw::PasswordEntryRow>,
        #[template_child]
        pub port_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub path_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub main_route_name_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub toast: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,

        #[template_child]
        pub nav: TemplateChild<adw::NavigationPage>,

        #[template_child]
        pub protocol: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub server_type: TemplateChild<gtk::DropDown>,

        #[template_child]
        pub routes_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub routes_listbox: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub route_dialog: TemplateChild<adw::Dialog>,
        #[template_child]
        pub route_name_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub route_server_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub route_protocol: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub route_port_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub route_path_entry: TemplateChild<adw::EntryRow>,

        #[property(get, set, builder(ActionType::default()))]
        pub action_type: Cell<ActionType>,
        pub old_account: RefCell<Option<Account>>,
        pub routes: RefCell<Vec<Route>>,
        pub editing_route_index: Cell<Option<usize>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AccountWindow {
        const NAME: &'static str = "AccountWindow";
        type Type = super::AccountWindow;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
            klass.install_action_async("account.add", None, |account, _, _| async move {
                account.add().await;
            });
        }

        fn instance_init(obj: &InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for AccountWindow {
        fn constructed(&self) {
            self.parent_constructed();
        }
    }

    impl WidgetImpl for AccountWindow {}
    impl AdwDialogImpl for AccountWindow {}
}

glib::wrapper! {
    pub struct AccountWindow(ObjectSubclass<imp::AccountWindow>)
    @extends gtk::Widget, adw::Dialog, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Root;
}

impl Default for AccountWindow {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize_path(path: &str) -> Option<String> {
    let path = path.trim().trim_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

fn normalize_optional_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn localize_input_error(message: &str) -> String {
    match message {
        "Server address is required" => gettext("Server address is required"),
        "Invalid server address" => gettext("Invalid server address"),
        "Route name is required" => gettext("Route name is required"),
        "Port must be between 1 and 65535" => gettext("Port must be between 1 and 65535"),
        _ => message.to_string(),
    }
}

fn normalize_server(protocol: u32, input: &str) -> Result<String, &'static str> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Server address is required");
    }

    let candidate = if input.starts_with("http://") || input.starts_with("https://") {
        input.to_string()
    } else {
        let scheme = if protocol == 1 { "https" } else { "http" };
        format!("{scheme}://{input}")
    };
    let mut url = url::Url::parse(&candidate).map_err(|_| "Invalid server address")?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err("Invalid server address");
    }

    url.set_port(None).map_err(|_| "Invalid server address")?;
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn build_route(
    name: &str, server: &str, protocol: u32, port: &str, path: &str,
) -> Result<Route, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Route name is required");
    }
    let port = port.trim();
    if port.parse::<u16>().ok().filter(|port| *port > 0).is_none() {
        return Err("Port must be between 1 and 65535");
    }

    Ok(Route {
        name: name.to_string(),
        server: normalize_server(protocol, server)?,
        port: port.to_string(),
        path: normalize_path(path),
    })
}

fn reconcile_active_route(old_account: Option<&Account>, routes: &[Route]) -> Option<usize> {
    let account = old_account?;
    let active = account.active_route?;
    let old_route = account.routes.get(active)?;

    if routes.get(active) == Some(old_route)
        || (account.routes.len() == routes.len() && active < routes.len())
    {
        Some(active)
    } else {
        routes.iter().position(|route| route == old_route)
    }
}

fn is_active_account(account: &Account, active_account: &Account) -> bool {
    !active_account.user_id.is_empty() && account == active_account
}

fn should_update_preferred_server(
    preferred_server: &str, old_account: &Account, new_account: &Account,
) -> bool {
    old_account.servername != new_account.servername && preferred_server == old_account.servername
}

fn route_subtitle(route: &Route) -> String {
    let path = route
        .path
        .as_deref()
        .map(|path| format!("/{path}"))
        .unwrap_or_default();
    format!("{}:{}{path}", route.server, route.port)
}

#[template_callbacks]
impl AccountWindow {
    pub fn new() -> Self {
        Object::builder().build()
    }

    #[template_callback]
    async fn on_password_entry_activated(&self) {
        self.add().await;
    }

    pub async fn add(&self) {
        let imp = self.imp();
        let mut servername = imp.servername_entry.text().to_string();
        let scheme = imp.protocol.selected();
        let server = imp.server_entry.text();
        let username = imp.username_entry.text();
        let password = imp.password_entry.text();
        let port = imp.port_entry.text();
        if server.is_empty() || username.is_empty() || port.is_empty() {
            imp.stack.toast(gettext("Fields must be filled in"));
            return;
        }

        imp.stack.set_visible_child_name("loading");

        let server = match normalize_server(scheme, &server) {
            Ok(server) => server,
            Err(message) => {
                imp.stack.toast(localize_input_error(message));
                imp.stack.set_visible_child_name("entry");
                return;
            }
        };

        let path = normalize_path(&imp.path_entry.text());
        let server_type = ServerType::from_index(imp.server_type.selected());
        let username = username.to_string();
        let password = password.to_string();
        let port = port.to_string();
        let discover_server_name = servername.is_empty();
        let server_for_login = server.clone();
        let port_for_login = port.clone();
        let path_for_login = path.clone();
        let username_for_login = username.clone();
        let password_for_login = password.clone();
        let (login, discovered_server_name) = match spawn_tokio(async move {
            let login = JELLYFIN_CLIENT
                .login(
                    &server_for_login,
                    &port_for_login,
                    path_for_login.as_deref(),
                    server_type,
                    &username_for_login,
                    &password_for_login,
                )
                .await?;
            let discovered_server_name = if discover_server_name {
                Some(
                    JELLYFIN_CLIENT
                        .get_server_info_public(
                            &server_for_login,
                            &port_for_login,
                            path_for_login.as_deref(),
                            server_type,
                        )
                        .await?
                        .server_name,
                )
            } else {
                None
            };
            Ok::<_, anyhow::Error>((login, discovered_server_name))
        })
        .await
        {
            Ok(result) => result,
            Err(e) => {
                imp.stack.toast(e.to_user_facing());
                imp.stack.set_visible_child_name("entry");
                return;
            }
        };

        if let Some(discovered_server_name) = discovered_server_name {
            servername = discovered_server_name;
        }

        let routes = imp.routes.borrow().clone();
        let active_route = reconcile_active_route(imp.old_account.borrow().as_ref(), &routes);

        let account = Account {
            servername,
            server,
            username,
            password,
            port,
            user_id: login.user.id,
            access_token: login.access_token,
            server_type: Some(server_type),
            path,
            route_name: normalize_optional_text(&imp.main_route_name_entry.text()),
            routes,
            active_route,
        };

        let action_type = imp.action_type.get();

        match action_type {
            ActionType::Edit => {
                let old_account = imp.old_account.borrow().clone().expect("No server to edit");
                let active_account = JELLYFIN_CLIENT.session().account.clone();
                let was_active = is_active_account(&old_account, &active_account);
                let preferred_server = SETTINGS.preferred_server();
                let update_preferred =
                    should_update_preferred_server(&preferred_server, &old_account, &account);

                if SETTINGS
                    .accounts()
                    .iter()
                    .any(|saved| saved != &old_account && saved.servername == account.servername)
                {
                    imp.stack
                        .toast(gettext("A server with this name already exists."));
                    imp.stack.set_visible_child_name("entry");
                    return;
                }

                let active_generation = if was_active {
                    let generation = match JELLYFIN_CLIENT.init_with_generation(&account).await {
                        Ok(generation) => generation,
                        Err(error) => {
                            imp.stack.toast(error.to_user_facing());
                            imp.stack.set_visible_child_name("entry");
                            return;
                        }
                    };
                    if let Err(error) = spawn_tokio(async move {
                        JELLYFIN_CLIENT
                            .authenticate_admin_for_generation(generation)
                            .await
                    })
                    .await
                    {
                        if JELLYFIN_CLIENT.is_session_generation_current(generation) {
                            let _ = JELLYFIN_CLIENT.init(&old_account).await;
                        }
                        imp.stack.toast(error.to_user_facing());
                        imp.stack.set_visible_child_name("entry");
                        return;
                    }
                    if !JELLYFIN_CLIENT.is_session_generation_current(generation) {
                        imp.stack
                            .toast(gettext("Account changed while validating the server."));
                        imp.stack.set_visible_child_name("entry");
                        return;
                    }
                    Some(generation)
                } else {
                    None
                };

                if let Err(error) = SETTINGS.edit_account(old_account.clone(), account.clone()) {
                    if active_generation.is_some_and(|generation| {
                        JELLYFIN_CLIENT.is_session_generation_current(generation)
                    }) {
                        let _ = JELLYFIN_CLIENT.init(&old_account).await;
                    }
                    imp.stack.toast(error.to_string());
                    imp.stack.set_visible_child_name("entry");
                    return;
                }

                if update_preferred
                    && let Err(error) = SETTINGS.set_preferred_server(&account.servername)
                {
                    let _ = SETTINGS.edit_account(account.clone(), old_account.clone());
                    if active_generation.is_some_and(|generation| {
                        JELLYFIN_CLIENT.is_session_generation_current(generation)
                    }) {
                        let _ = JELLYFIN_CLIENT.init(&old_account).await;
                    }
                    imp.stack.toast(error.to_string());
                    imp.stack.set_visible_child_name("entry");
                    return;
                }

                imp.old_account.take();
                self.close_dialog(&gettext("Server edited successfully"))
                    .await;
            }
            ActionType::Add => {
                SETTINGS.add_account(account).expect("Failed to add server");
                self.close_dialog(&gettext("Server added successfully"))
                    .await;
            }
        }
    }

    async fn close_dialog(&self, msg: &str) {
        self.imp().stack.set_visible_child_name("entry");
        self.close();
        let root = self.root();
        let window = root.and_downcast_ref::<super::window::Window>().unwrap();
        self.toast(msg);
        window.set_servers().await;
        window.set_nav_servers();
        window.update_route_switcher();
    }

    #[template_callback]
    fn on_server_entry_changed(&self, entry: &adw::EntryRow) {
        let text = entry.text().to_string();

        let Some(url) = url::Url::parse(&text).ok() else {
            return;
        };

        // Prevent Gtk-WARNING **: Cannot begin irreversible action while in user action
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            move || {
                obj.parse_url(&url);
            }
        ));
    }

    fn parse_url(&self, url: &url::Url) {
        let (protocol_idx, default_port) = match url.scheme() {
            "http" => (0, 80),
            "https" => (1, 443),
            _ => return,
        };

        self.imp().protocol.set_selected(protocol_idx);
        self.imp()
            .port_entry
            .set_text(&url.port().unwrap_or(default_port).to_string());

        if let Some(host) = url.host_str() {
            self.imp().server_entry.set_text(host);
            self.imp().server_entry.set_position(-1);
        }

        if let Some(path) = normalize_path(url.path()) {
            self.imp().path_entry.set_text(&path);
        }
    }

    pub fn set_routes(&self, routes: Vec<Route>) {
        self.imp().routes.replace(routes);
        self.refresh_routes();
    }

    fn refresh_routes(&self) {
        let imp = self.imp();
        imp.routes_listbox.remove_all();

        let routes = imp.routes.borrow();
        imp.routes_stack
            .set_visible_child_name(if routes.is_empty() { "empty" } else { "list" });

        for (index, route) in routes.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(&route.name)
                .subtitle(route_subtitle(route))
                .activatable(true)
                .build();
            let delete_button = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(gettext("Delete Route"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();

            delete_button.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                move |_| {
                    if index < obj.imp().routes.borrow().len() {
                        obj.imp().routes.borrow_mut().remove(index);
                        obj.refresh_routes();
                    }
                }
            ));
            row.add_suffix(&delete_button);
            row.connect_activated(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                move |_| obj.open_route_dialog(Some(index))
            ));
            imp.routes_listbox.append(&row);
        }
    }

    fn open_route_dialog(&self, index: Option<usize>) {
        let imp = self.imp();
        imp.editing_route_index.set(index);

        if let Some(route) = index.and_then(|index| imp.routes.borrow().get(index).cloned()) {
            let url = url::Url::parse(&route.server).ok();
            imp.route_name_entry.set_text(&route.name);
            imp.route_protocol
                .set_selected(url.as_ref().is_some_and(|url| url.scheme() == "https") as u32);
            let host = url
                .as_ref()
                .and_then(url::Url::host)
                .map_or_else(|| route.server.clone(), |host| host.to_string());
            imp.route_server_entry.set_text(host.as_str());
            imp.route_port_entry.set_text(&route.port);
            imp.route_path_entry
                .set_text(route.path.as_deref().unwrap_or_default());
        } else {
            imp.route_name_entry.set_text("");
            imp.route_server_entry.set_text("");
            imp.route_protocol.set_selected(0);
            imp.route_port_entry.set_text("");
            imp.route_path_entry.set_text("");
        }

        imp.route_dialog.present(Some(self));
    }

    #[template_callback]
    fn on_add_route_clicked(&self) {
        self.open_route_dialog(None);
    }

    #[template_callback]
    fn on_route_cancel_clicked(&self) {
        self.imp().editing_route_index.set(None);
        self.imp().route_dialog.close();
    }

    #[template_callback]
    fn on_route_save_clicked(&self) {
        let imp = self.imp();
        let route = match build_route(
            &imp.route_name_entry.text(),
            &imp.route_server_entry.text(),
            imp.route_protocol.selected(),
            &imp.route_port_entry.text(),
            &imp.route_path_entry.text(),
        ) {
            Ok(route) => route,
            Err(message) => {
                self.toast(localize_input_error(message));
                return;
            }
        };

        let index = imp.editing_route_index.take();
        let mut routes = imp.routes.borrow_mut();
        if let Some(index) = index.filter(|index| *index < routes.len()) {
            routes[index] = route;
        } else {
            routes.push(route);
        }
        drop(routes);

        imp.route_dialog.close();
        self.refresh_routes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(servername: &str) -> Account {
        Account {
            servername: servername.into(),
            user_id: "user-id".into(),
            access_token: "token".into(),
            ..Default::default()
        }
    }

    #[test]
    fn active_account_requires_the_exact_initialized_account() {
        let saved = account("Home");
        assert!(is_active_account(&saved, &saved));

        let mut different_route = saved.clone();
        different_route.active_route = Some(0);
        assert!(!is_active_account(&saved, &different_route));
        assert!(!is_active_account(&saved, &Account::default()));
    }

    #[test]
    fn preferred_server_follows_an_account_rename() {
        let old_account = account("Home");
        let renamed_account = account("Media");

        assert!(should_update_preferred_server(
            "Home",
            &old_account,
            &renamed_account
        ));
        assert!(!should_update_preferred_server(
            "Other",
            &old_account,
            &renamed_account
        ));
        assert!(!should_update_preferred_server(
            "Home",
            &old_account,
            &old_account
        ));
    }

    #[test]
    fn route_input_is_normalized() {
        let route = build_route(" Remote ", "example.com", 1, "8443", "/jellyfin/").unwrap();

        assert_eq!(route.name, "Remote");
        assert_eq!(route.server, "https://example.com");
        assert_eq!(route.port, "8443");
        assert_eq!(route.path.as_deref(), Some("jellyfin"));
    }

    #[test]
    fn active_route_tracks_deletions_before_it() {
        let first = Route {
            name: "First".into(),
            server: "https://first.example".into(),
            port: "443".into(),
            path: None,
        };
        let second = Route {
            name: "Second".into(),
            server: "https://second.example".into(),
            port: "443".into(),
            path: None,
        };
        let account = Account {
            routes: vec![first, second.clone()],
            active_route: Some(1),
            ..Default::default()
        };

        assert_eq!(reconcile_active_route(Some(&account), &[second]), Some(0));
    }
}
