use std::cell::RefCell;

use gtk::{
    glib,
    glib::{
        prelude::*,
        subclass::prelude::*,
    },
};

use crate::client::Account;

pub mod imp {
    use gtk::glib::Properties;

    use super::*;

    #[derive(Properties, Default)]
    #[properties(wrapper_type = super::AccountItem)]
    pub struct AccountItem {
        #[property(get, set)]
        server: RefCell<String>,
        #[property(get, set)]
        servername: RefCell<String>,
        #[property(get, set)]
        username: RefCell<String>,
        #[property(get, set)]
        password: RefCell<String>,
        #[property(get, set)]
        port: RefCell<String>,
        #[property(get, set)]
        user_id: RefCell<String>,
        #[property(get, set)]
        access_token: RefCell<String>,
        #[property(get, set)]
        server_type: RefCell<Option<String>>,
        pub inner: RefCell<Option<Account>>,
    }

    #[glib::derived_properties]
    impl ObjectImpl for AccountItem {}

    #[glib::object_subclass]
    impl ObjectSubclass for AccountItem {
        const NAME: &'static str = "AccountItem";
        type Type = super::AccountItem;
    }
}

glib::wrapper! {
    pub struct AccountItem(ObjectSubclass<imp::AccountItem>);
}

impl AccountItem {
    pub fn from_simple(account: &Account) -> Self {
        let item: AccountItem = glib::object::Object::new();
        item.set_server(account.server.clone());
        item.set_servername(account.servername.clone());
        item.set_username(account.username.clone());
        item.set_password(account.password.clone());
        item.set_port(account.port.clone());
        item.set_user_id(account.user_id.clone());
        item.set_access_token(account.access_token.clone());
        if let Some(server_type) = &account.server_type {
            item.set_server_type(server_type.clone());
        }
        item.imp().inner.replace(Some(account.to_owned()));
        item
    }

    pub fn account(&self) -> Account {
        if let Some(account) = self.imp().inner.borrow().as_ref() {
            return Account {
                server: self.server(),
                servername: self.servername(),
                username: self.username(),
                password: self.password(),
                port: self.port(),
                user_id: self.user_id(),
                access_token: self.access_token(),
                server_type: self.server_type(),
                path: account.path.clone(),
                route_name: account.route_name.clone(),
                routes: account.routes.clone(),
                active_route: account.active_route,
            };
        }
        Account {
            server: self.server(),
            servername: self.servername(),
            username: self.username(),
            password: self.password(),
            port: self.port(),
            user_id: self.user_id(),
            access_token: self.access_token(),
            server_type: self.server_type(),
            path: None,
            route_name: None,
            routes: Vec::new(),
            active_route: None,
        }
    }
}
