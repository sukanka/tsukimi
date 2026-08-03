use gettextrs::gettext;
use gtk::{
    gio,
    prelude::*,
};

use crate::{
    client::{
        Account,
        jellyfin_client::JELLYFIN_CLIENT,
    },
    ui::models::SETTINGS,
};

pub fn current_account() -> Option<Account> {
    let accounts = SETTINGS.accounts();
    let session_account = JELLYFIN_CLIENT.session().account.clone();
    if !session_account.user_id.is_empty() {
        return accounts
            .into_iter()
            .find(|account| account == &session_account);
    }

    let preferred = SETTINGS.preferred_server();
    accounts
        .into_iter()
        .find(|account| account.servername == preferred)
}

fn route_name(account: &Account, route_index: Option<usize>) -> String {
    let Some(index) = route_index else {
        return account.main_route_name().to_string();
    };
    account
        .routes
        .get(index)
        .map(|route| {
            let name = route.name.trim();
            if name.is_empty() {
                format!("{} {}", gettext("Route"), index + 1)
            } else {
                name.to_string()
            }
        })
        .unwrap_or_else(|| account.main_route_name().to_string())
}

fn menu_label(account: &Account, route_index: Option<usize>) -> String {
    let marker = if account.active_route == route_index {
        "✓"
    } else {
        " "
    };
    format!("{marker} {}", route_name(account, route_index))
}

pub fn build_route_menu(account: &Account) -> gio::Menu {
    let menu = gio::Menu::new();

    let main = gio::MenuItem::new(Some(&menu_label(account, None)), None);
    main.set_action_and_target_value(Some("win.switch-route"), Some(&(-1_i32).to_variant()));
    menu.append_item(&main);

    for index in 0..account.routes.len() {
        let item = gio::MenuItem::new(Some(&menu_label(account, Some(index))), None);
        item.set_action_and_target_value(
            Some("win.switch-route"),
            Some(&(index as i32).to_variant()),
        );
        menu.append_item(&item);
    }

    menu
}

pub fn setup_route_switch_button(button: &gtk::MenuButton) {
    refresh_route_switch_button(button, current_account().as_ref());
}

pub fn refresh_route_switch_button(button: &gtk::MenuButton, account: Option<&Account>) {
    if let Some(account) = account {
        button.set_label(&route_name(account, account.active_route));
        button.set_menu_model(Some(&build_route_menu(account)));
        button.set_visible(true);
    } else {
        button.set_menu_model(None::<&gio::MenuModel>);
        button.set_visible(false);
    }
}
