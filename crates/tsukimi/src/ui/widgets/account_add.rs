use adw::prelude::*;
use anyhow::{
    Result,
    ensure,
};
use gettextrs::gettext;
use glib::Object;
use gtk::{
    glib,
    subclass::prelude::*,
    template_callbacks,
};
use std::sync::Arc;
use url::Url;

use super::{
    utils::GlobalToast,
    window::Window,
};
use crate::{
    client::{
        Account,
        account::{
            Route,
            ServerType,
        },
        error::UserFacingError,
        jellyfin_client::JELLYFIN_CLIENT,
    },
    ui::models::SETTINGS,
    utils::spawn_tokio,
};

struct RouteRow {
    row: adw::ExpanderRow,
    name: adw::EntryRow,
    address: adw::EntryRow,
    selected: bool,
}

mod imp {
    use std::cell::RefCell;

    use adw::subclass::dialog::AdwDialogImpl;
    use glib::subclass::InitializingObject;
    use gtk::{
        CompositeTemplate,
        glib,
        subclass::prelude::*,
    };

    use super::RouteRow;
    use crate::client::Account;

    #[derive(CompositeTemplate, Default)]
    #[template(resource = "/moe/tsuna/tsukimi/ui/account.ui")]
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
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub nav: TemplateChild<adw::NavigationPage>,
        #[template_child]
        pub protocol: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub server_type: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub path_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub route_name_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub routes_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub save_row: TemplateChild<adw::ButtonRow>,

        pub old_account: RefCell<Option<Account>>,
        pub(super) routes: RefCell<Vec<RouteRow>>,
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

    impl ObjectImpl for AccountWindow {}
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

#[template_callbacks]
impl AccountWindow {
    pub fn new() -> Self {
        Object::builder().build()
    }

    pub fn for_edit(account: Account) -> Self {
        let dialog = Self::new();
        let imp = dialog.imp();
        imp.nav.set_title(&gettext("Edit Server"));
        imp.save_row.set_title(&gettext("Save"));
        imp.servername_entry.set_text(&account.servername);
        imp.username_entry.set_text(&account.username);
        imp.password_entry.set_text(&account.password);
        imp.server_type
            .set_selected(account.server_type.unwrap_or_default().index());
        imp.route_name_entry
            .set_text(account.route_name.as_deref().unwrap_or_default());
        if let Ok(url) = account.main_url() {
            dialog.fill_endpoint(&url);
        }
        for (index, route) in account.routes.iter().enumerate() {
            dialog.append_route(Some(route), account.active_route == Some(index));
        }
        imp.old_account.replace(Some(account));
        dialog
    }

    #[template_callback]
    fn on_password_entry_activated(&self) {
        let _ = self.activate_action("account.add", None);
    }

    async fn add(&self) {
        let imp = self.imp();
        if imp.stack.visible_child_name().as_deref() != Some("entry") {
            return;
        }
        let old_account = imp.old_account.borrow().clone();
        let draft = match self.draft(old_account.as_ref()) {
            Ok(draft) => draft,
            Err(error) => {
                imp.stack.toast(error.to_user_facing());
                return;
            }
        };
        let window = self.root().and_downcast::<Window>().unwrap();
        imp.stack.set_visible_child_name("loading");
        self.action_set_enabled("account.add", false);
        let result = Self::save(old_account.as_ref(), draft).await;
        self.action_set_enabled("account.add", true);
        imp.stack.set_visible_child_name("entry");
        if let Err(error) = result {
            imp.stack.toast(error.to_user_facing());
            return;
        }

        self.close();
        window.toast(if old_account.is_some() {
            gettext("Server edited successfully")
        } else {
            gettext("Server added successfully")
        });
        window.set_servers().await;
        window.set_nav_servers();
    }

    fn draft(&self, old_account: Option<&Account>) -> Result<Account> {
        let imp = self.imp();
        if let Ok(url) = Url::parse(imp.server_entry.text().trim()) {
            self.fill_endpoint(&url);
        }
        let server = imp.server_entry.text();
        let port = imp.port_entry.text();
        let username = imp.username_entry.text();
        ensure!(
            !server.trim().is_empty() && !port.trim().is_empty() && !username.is_empty(),
            gettext("Fields must be filled in")
        );
        let scheme = if imp.protocol.selected() == 0 {
            "http"
        } else {
            "https"
        };
        let server_type = ServerType::from_index(imp.server_type.selected());
        let route_name = imp.route_name_entry.text();
        let path = imp.path_entry.text();
        let path = path.trim().trim_matches('/');
        let rows = imp.routes.borrow();
        let routes = rows
            .iter()
            .map(|row| {
                let name = row.name.text().trim().to_owned();
                ensure!(!name.is_empty(), gettext("Route name is required"));
                Route::from_url(name, row.address.text().trim())
            })
            .collect::<Result<Vec<_>>>()?;
        let mut account = Account {
            servername: imp.servername_entry.text().trim().to_owned(),
            server: format!("{scheme}://{}", server.trim()),
            port: port.trim().to_owned(),
            username: username.to_string(),
            password: imp.password_entry.text().to_string(),
            server_type: Some(server_type),
            route_name: (!route_name.trim().is_empty()).then(|| route_name.trim().to_owned()),
            path: (!path.is_empty()).then(|| path.to_owned()),
            routes,
            active_route: rows.iter().position(|row| row.selected),
            ..Default::default()
        };
        account.main_url()?;
        if let Some(old) = old_account
            && old.server_type.unwrap_or_default() == server_type
        {
            account.server_type = old.server_type;
            if account.username == old.username && account.password == old.password {
                account.user_id.clone_from(&old.user_id);
                account.access_token.clone_from(&old.access_token);
            }
        }
        Ok(account)
    }

    async fn save(old_account: Option<&Account>, draft: Account) -> Result<()> {
        let (session, user) =
            spawn_tokio(async move { JELLYFIN_CLIENT.validate_account(draft).await }).await?;
        let current = JELLYFIN_CLIENT.session();
        let is_active = current.url_headers.is_some()
            && match old_account {
                Some(old) => old == &current.account,
                None => session.account.servername == current.account.servername,
            };
        if let Some(old) = old_account {
            SETTINGS.edit_account(old, &session.account)?;
        } else {
            SETTINGS.add_account(session.account.clone())?;
        }
        if is_active {
            JELLYFIN_CLIENT.install_session(Arc::new(session), user.policy.is_administrator);
        }
        Ok(())
    }

    #[template_callback]
    fn on_server_entry_changed(&self, entry: &adw::EntryRow) {
        let Ok(url) = Url::parse(entry.text().trim()) else {
            return;
        };

        // Avoid changing the entry during its current edit action.
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            move || obj.fill_endpoint(&url)
        ));
    }

    fn fill_endpoint(&self, url: &Url) {
        let (protocol, port) = match url.scheme() {
            "http" => (0, 80),
            "https" => (1, 443),
            _ => return,
        };
        let imp = self.imp();
        imp.protocol.set_selected(protocol);
        imp.port_entry
            .set_text(&url.port().unwrap_or(port).to_string());
        imp.server_entry.set_text(url.host_str().unwrap());
        imp.server_entry.set_position(-1);
        let path = url.path().trim_matches('/');
        if !path.is_empty() {
            imp.path_entry.set_text(path);
        }
    }

    #[template_callback]
    fn on_add_route_clicked(&self) {
        self.append_route(None, false);
    }

    fn append_route(&self, route: Option<&Route>, selected: bool) {
        let imp = self.imp();
        let name = adw::EntryRow::builder().title(gettext("Name")).build();
        let address = adw::EntryRow::builder()
            .title(gettext("Server URL"))
            .build();
        let title = gettext("Route");
        let row = adw::ExpanderRow::builder()
            .title(&title)
            .expanded(route.is_none())
            .use_markup(false)
            .build();
        row.add_row(&name);
        row.add_row(&address);
        name.connect_changed(glib::clone!(
            #[weak]
            row,
            move |entry| {
                let text = entry.text();
                row.set_title(if text.is_empty() {
                    &title
                } else {
                    text.as_str()
                });
            }
        ));
        if let Some(route) = route {
            name.set_text(&route.name);
            if let Ok(url) = route.url() {
                address.set_text(url.as_str());
            }
        }

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(gettext("Delete Route"))
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        remove.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            #[weak]
            row,
            move |_| {
                let imp = dialog.imp();
                imp.routes_group.remove(&row);
                imp.routes.borrow_mut().retain(|route| route.row != row);
            }
        ));
        row.add_suffix(&remove);
        imp.routes_group.add(&row);
        imp.routes.borrow_mut().push(RouteRow {
            row,
            name,
            address,
            selected,
        });
    }
}
