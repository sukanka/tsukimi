use gettextrs::gettext;
use gtk::{
    gio,
    prelude::*,
};

use crate::{
    client::Account,
    ui::models::SETTINGS,
};

pub fn current_account() -> Option<Account> {
    let preferred = SETTINGS.preferred_server();
    SETTINGS
        .accounts()
        .into_iter()
        .find(|a| a.servername == preferred)
}

pub fn build_route_menu(account: &Account) -> gio::Menu {
    let menu = gio::Menu::new();
    let current = account.active_route;

    let main_label = if current.is_none() {
        format!("● {}", account.main_route_name())
    } else {
        format!("  {}", account.main_route_name())
    };
    let main_item = gio::MenuItem::new(Some(&main_label), None);
    main_item.set_action_and_target_value(
        Some("win.switch-route"),
        Some(&(-1i32).to_variant()),
    );
    menu.append_item(&main_item);

    for (i, route) in account.routes.iter().enumerate() {
        let name = if route.name.trim().is_empty() {
            format!("{} {}", gettext("Route"), i + 1)
        } else {
            route.name.clone()
        };
        let label = if current == Some(i) {
            format!("● {name}")
        } else {
            format!("  {name}")
        };
        let item = gio::MenuItem::new(Some(&label), None);
        item.set_action_and_target_value(
            Some("win.switch-route"),
            Some(&(i as i32).to_variant()),
        );
        menu.append_item(&item);
    }

    menu
}

pub fn setup_route_switch_button(button: &gtk::MenuButton) {
    refresh_route_switch_button(button, current_account().as_ref());
}

pub fn refresh_route_switch_button(button: &gtk::MenuButton, account: Option<&Account>) {
    match account {
        Some(account) if !account.routes.is_empty() => {
            let label = match account.active_route {
                Some(i) => account
                    .routes
                    .get(i)
                    .map(|r| {
                        if r.name.trim().is_empty() {
                            format!("{} {}", gettext("Route"), i + 1)
                        } else {
                            r.name.clone()
                        }
                    })
                    .unwrap_or_else(|| account.main_route_name().to_string()),
                None => account.main_route_name().to_string(),
            };
            button.set_label(&label);
            button.set_menu_model(Some(&build_route_menu(account)));
            button.set_visible(true);
        }
        _ => {
            button.set_visible(false);
        }
    }
}
