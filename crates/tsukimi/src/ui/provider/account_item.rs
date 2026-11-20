use std::cell::RefCell;

use gtk::glib::{
    self,
    subclass::prelude::*,
};

use crate::client::Account;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct AccountItem(pub RefCell<Account>);

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
    pub fn from_simple(account: Account) -> Self {
        let item: Self = glib::Object::new();
        item.imp().0.replace(account);
        item
    }

    pub fn account(&self) -> Account {
        self.imp().0.borrow().clone()
    }
}
