//! Tray menu: one `CheckMenuItem` per jack, dynamic rebuild, and the real
//! toggle pipeline driving `AppState::set_patched` (MASTER_PLAN S5).
//!
//! ## muda gotcha — treat a click as *intent only*
//! Clicking a `CheckMenuItem` flips its checked state IN THE TOOLKIT before our
//! handler runs. So on a `jack:<name>` event we do NOT trust the menu item's
//! checked state; instead we read the AUTHORITATIVE `patched` flag from
//! `AppState`, compute the opposite, drive `set_patched`, then reconcile
//! `set_checked` with the resulting flag. A failed ON keeps the box checked
//! (`patched` stays `true`, status = `Failed`) and the tooltip reflects it.
//!
//! Menu ids: `"jack:<name>"` per jack, plus the fixed ids below.

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::tray::{TrayIcon, TrayIconId};
use tauri::{AppHandle, Manager, Wry};

use crate::app_state::{AppState, GatewayStatus, ToggleResult};
use crate::config;
use crate::gateway;
use crate::utils::autorun;
use crate::utils::log::log;

/// Fixed tray id so handlers can look the icon up to swap menus / set tooltip.
pub const TRAY_ID: &str = "main";

/// Fixed menu ids (jacks use `"jack:<name>"`).
const ID_AUTOSTART: &str = "autostart";
const ID_REQUIRE_APPROVAL: &str = "require_approval";
const ID_REQUEST_LOGGING: &str = "request_logging";
const ID_RELOAD: &str = "reload";
const ID_RETRY_GATEWAY: &str = "retry_gateway";
const ID_OPEN_CONFIG: &str = "open_config";
const ID_OPEN_LOGS: &str = "open_logs";
const ID_COPY_URL: &str = "copy_url";
const ID_ABOUT: &str = "about";
const ID_QUIT: &str = "quit";

/// "About" text, written fresh to `About.txt` (next to `patchbay.json`) on
/// every click so it always matches the running exe, then opened in the OS
/// default text editor via the same pattern as [`on_open_config`].
const ABOUT_TEXT: &str = include_str!("../../ABOUT.txt");

/// One per-client-per-jack override checkbox (S10). Carries the opaque muda
/// menu item plus the (client, jack) it represents, so a click on an opaque id
/// can be resolved back to its meaning via the [`TrayItems::overrides`] lookup
/// table — avoiding any fragile encoding of two arbitrary strings into one id.
#[derive(Clone)]
pub struct OverrideItem {
    pub item: CheckMenuItem<Wry>,
    pub client: String,
    pub jack: String,
}

/// One per-identity forbidden toggle checkbox (S11 redesign of the "Forbidden"
/// submenu). Carries the opaque muda menu item plus the identity it represents,
/// so a click on an opaque `"forbid:<n>"` id can be resolved back to its
/// meaning via the [`TrayItems::forbidden`] lookup table AND reconciled
/// (`set_checked` + `set_text`) after a toggle.
#[derive(Clone)]
pub struct ForbiddenItem {
    pub item: CheckMenuItem<Wry>,
    pub identity: String,
}

/// One per-client "Enable Custom permissions" checkbox (S11). The FIRST item in
/// each agent's own submenu under "Custom (N)", making "which agents get Custom
/// mode" a deliberate action. Carries the opaque muda menu item plus the client
/// name it represents, keyed by an opaque `"enable_custom:<n>"` id in the
/// [`TrayItems::enable_custom`] lookup table.
#[derive(Clone)]
pub struct EnableCustomItem {
    pub item: CheckMenuItem<Wry>,
    pub client: String,
}

/// One per-agent "✕ Delete this agent" item (S12), living ONLY in the "Custom"
/// submenu as the LAST entry of each agent's own submenu. A plain (non-checkbox)
/// `MenuItem` because it's a one-shot destructive action (purge the identity
/// from `seen_clients` + `client_overrides` + `forbidden_clients`), not a state
/// toggle — so there is no check to reconcile; the whole menu is rebuilt after a
/// confirmed delete. Carries the identity it represents, keyed by an opaque
/// `"delete_client:<n>"` id in the [`TrayItems::delete_client`] lookup table
/// (the identity string is resolved from the table, never encoded in the id).
#[derive(Clone)]
pub struct DeleteClientItem {
    pub identity: String,
}

/// Holds the live `CheckMenuItem` handles so click handlers can reconcile
/// `set_checked` after a toggle drives the authoritative flag. Jack checkboxes
/// (+ the autostart/approval items) are keyed by jack name / their fixed id in
/// `checks`; per-client-per-jack override checkboxes (S10) are keyed by an
/// OPAQUE menu id (`"ovr:<n>"`) in `overrides`, with the meaning held in the
/// [`OverrideItem`]; per-identity forbidden TOGGLE checkboxes (S11 redesign of
/// the "Forbidden (N)" submenu) are keyed by an OPAQUE menu id
/// (`"forbid:<n>"`) in `forbidden`, with the meaning held in the
/// [`ForbiddenItem`]; per-client "Enable Custom permissions" checkboxes (S11)
/// are keyed by an OPAQUE menu id (`"enable_custom:<n>"`) in `enable_custom`,
/// with the meaning held in the [`EnableCustomItem`]; per-agent "✕ Delete this
/// agent" plain menu items (S12, Custom submenu only) are keyed by an OPAQUE
/// menu id (`"delete_client:<n>"`) in `delete_client`, with the meaning held in
/// the [`DeleteClientItem`].
#[derive(Default)]
pub struct TrayItems {
    pub checks: Mutex<HashMap<String, CheckMenuItem<Wry>>>,
    /// S10 override checkboxes, keyed by opaque menu id.
    pub overrides: Mutex<HashMap<String, OverrideItem>>,
    /// Monotonic counter for S10 override menu ids (unique + stable within one
    /// build, reset each rebuild).
    pub override_counter: Mutex<u64>,
    /// S11 "Forbidden" per-identity toggle checkboxes, keyed by opaque menu id
    /// (`"forbid:<n>"`) -> the identity that entry toggles.
    pub forbidden: Mutex<HashMap<String, ForbiddenItem>>,
    /// Monotonic counter for forbidden-entry menu ids (reset each rebuild).
    pub forbidden_counter: Mutex<u64>,
    /// S11 "Enable Custom permissions" checkboxes, keyed by opaque menu id
    /// (`"enable_custom:<n>"`) -> the client name that entry toggles Custom
    /// mode for.
    pub enable_custom: Mutex<HashMap<String, EnableCustomItem>>,
    /// Monotonic counter for enable-custom menu ids (reset each rebuild).
    pub enable_custom_counter: Mutex<u64>,
    /// S12 "✕ Delete this agent" items, keyed by opaque menu id
    /// (`"delete_client:<n>"`) -> the identity that entry purges (Custom
    /// submenu only).
    pub delete_client: Mutex<HashMap<String, DeleteClientItem>>,
    /// Monotonic counter for delete-client menu ids (reset each rebuild).
    pub delete_client_counter: Mutex<u64>,
}

// ---- public entrypoints (called from main.rs setup) ---------------------

/// Build the full tray menu from the live config: one `CheckMenuItem` per jack
/// (checked = `patched`), then a separator, a "Settings" submenu, "About
/// [<version>]" (top-level, directly under Settings), another separator, and
/// "Quit". The "Settings" submenu nests "Custom"/"Forbidden" per-agent
/// submenus, "Retry gateway", "Reload config", "Open config file", "Copy
/// gateway URL" (a separator), and the "Start with Windows"/"Require approval
/// for new agents" checks — so the top-level popup stays short.
/// Resets and repopulates the managed [`TrayItems`] handle map.
pub fn build_menu(app: &AppHandle, state: &AppState) -> Result<Menu<Wry>, tauri::Error> {
    // Reset the handle maps (rebuilt below).
    if let Some(items) = app.try_state::<TrayItems>() {
        items.checks.lock().clear();
        items.overrides.lock().clear();
        items.forbidden.lock().clear();
        items.enable_custom.lock().clear();
        items.delete_client.lock().clear();
        *items.override_counter.lock() = 0;
        *items.forbidden_counter.lock() = 0;
        *items.enable_custom_counter.lock() = 0;
        *items.delete_client_counter.lock() = 0;
    }

    let autostart = state.config.read().autostart;

    // ---- per-jack check items (checked reflects `patched`) ----
    let mut jack_items: Vec<CheckMenuItem<Wry>> = Vec::new();
    for line in state.jack_lines() {
        let item = CheckMenuItem::with_id(
            app,
            format!("jack:{}", line.name),
            &line.name,
            true,
            line.patched,
            None::<&str>,
        )?;
        if let Some(items) = app.try_state::<TrayItems>() {
            items.checks.lock().insert(line.name.clone(), item.clone());
        }
        jack_items.push(item);
    }

    // ---- Settings submenu: gateway/config actions + autostart toggle ----
    // "Retry gateway" re-attempts the bind after a port conflict / failed
    // startup (S7). Always present; a no-op while the gateway is healthy.
    let retry_gateway =
        MenuItem::with_id(app, ID_RETRY_GATEWAY, "Retry gateway", true, None::<&str>)?;
    let reload = MenuItem::with_id(app, ID_RELOAD, "Reload config", true, None::<&str>)?;
    let open_config =
        MenuItem::with_id(app, ID_OPEN_CONFIG, "Open config file", true, None::<&str>)?;
    let open_logs =
        MenuItem::with_id(app, ID_OPEN_LOGS, "Open logs folder", true, None::<&str>)?;
    let copy_url = MenuItem::with_id(app, ID_COPY_URL, "Copy gateway URL", true, None::<&str>)?;
    let autostart_item = CheckMenuItem::with_id(
        app,
        ID_AUTOSTART,
        "Start with Windows",
        true,
        autostart,
        None::<&str>,
    )?;
    // (S10c) "Require approval for new agents" check — default OFF, matching
    // `require_approval_for_new_clients`. Same click-then-reconcile pattern as
    // autostart: flips + persists the config field, no other side effects.
    let require_approval_on = state.config.read().require_approval_for_new_clients;
    let require_approval_item = CheckMenuItem::with_id(
        app,
        ID_REQUIRE_APPROVAL,
        "Require approval for new agents",
        true,
        require_approval_on,
        None::<&str>,
    )?;
    // "Enable request logging" check — bound to config.request_logging_enabled
    // (default OFF). Same click-then-reconcile pattern as autostart/approval:
    // the click flips + persists the flag (save-then-commit via the app_state
    // setter), then the box is reconciled to the AUTHORITATIVE resulting value.
    let request_logging_on = state.config.read().request_logging_enabled;
    let request_logging_item = CheckMenuItem::with_id(
        app,
        ID_REQUEST_LOGGING,
        "Enable request logging",
        true,
        request_logging_on,
        None::<&str>,
    )?;
    if let Some(items) = app.try_state::<TrayItems>() {
        items
            .checks
            .lock()
            .insert(ID_AUTOSTART.to_string(), autostart_item.clone());
        items
            .checks
            .lock()
            .insert(ID_REQUIRE_APPROVAL.to_string(), require_approval_item.clone());
        items
            .checks
            .lock()
            .insert(ID_REQUEST_LOGGING.to_string(), request_logging_item.clone());
    }
    let sep_autostart = PredefinedMenuItem::separator(app)?;

    // ---- S10/S11: "Custom (N)" submenu — per-agent on/off lists ----
    // N = number of clients currently with an ENABLED override. Hovering opens a
    // nested submenu listing every seen client; each client is itself a submenu
    // headed by an explicit "Enable Custom permissions" checkbox (S11), a
    // separator, then one CheckMenuItem per global jack reflecting that client's
    // effective value (effective_patched). Toggling a jack lazily creates that
    // client's override (Custom mode) on first use; the explicit Enable checkbox
    // makes choosing which agents get Custom mode a deliberate action.
    let custom_submenu = build_custom_submenu(app, state)?;

    // ---- S10c/S11: "Forbidden (N)" submenu — toggle any known agent on/off ----
    // N = number of identities in forbidden_clients (currently-denied count).
    // Lists EVERY seen client as a CheckMenuItem (checked = currently denied,
    // label "<name> [denied]"/"<name> [allowed]"); toggling drives
    // AppState::set_forbidden. Keyed by an opaque "forbid:<n>" id so the
    // identity string is resolved from the lookup table (never encoded in the id).
    let forbidden_submenu = build_forbidden_submenu(app, state)?;

    // Collect references of mixed menu-item types into one trait-object slice
    // (Submenu::with_items takes &[&dyn IsMenuItem], same as Menu::with_items).
    let mut settings_entries: Vec<&dyn IsMenuItem<Wry>> = Vec::new();
    settings_entries.push(&custom_submenu);
    settings_entries.push(&forbidden_submenu);
    settings_entries.push(&retry_gateway);
    settings_entries.push(&reload);
    settings_entries.push(&open_config);
    settings_entries.push(&open_logs);
    settings_entries.push(&copy_url);
    settings_entries.push(&sep_autostart);
    settings_entries.push(&autostart_item);
    settings_entries.push(&require_approval_item);
    settings_entries.push(&request_logging_item);

    let settings = Submenu::with_items(app, "Settings", true, &settings_entries)?;

    // ---- top-level menu: jacks / separator / Settings / About / separator / Quit ----
    // "About [<version>]" lives at the TOP LEVEL, directly under "Settings" (not
    // nested inside it), so it's a one-click destination. The version suffix is
    // read live from Cargo.toml/tauri.conf.json via `app.package_info()` — never
    // hardcoded here, so it can't drift from the actual build.
    let about_label = format!("About [{}]", app.package_info().version);
    let about = MenuItem::with_id(app, ID_ABOUT, about_label.as_str(), true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Quit", true, None::<&str>)?;

    let mut entries: Vec<&dyn IsMenuItem<Wry>> = Vec::new();
    for j in &jack_items {
        entries.push(j);
    }
    entries.push(&sep1);
    entries.push(&settings);
    entries.push(&about);
    entries.push(&sep2);
    entries.push(&quit);

    Menu::with_items(app, &entries)
}

/// Build the S10 "Custom (N)" submenu: a nested list of every seen client,
/// each itself a submenu headed by an explicit "Enable Custom permissions"
/// checkbox (S11), a separator, then one CheckMenuItem per global jack
/// reflecting that client's effective value. N = count of ENABLED overrides.
/// Per-jack override checkboxes use OPAQUE ids (`ovr:<n>`) registered in the
/// [`TrayItems::overrides`] table; the per-client Enable checkbox uses OPAQUE
/// ids (`enable_custom:<n>`) registered in [`TrayItems::enable_custom`] — so the
/// arbitrary external `clientInfo.name` never has to be encoded into an id, and
/// the two click kinds are unambiguous in `on_menu_event`.
fn build_custom_submenu(app: &AppHandle, state: &AppState) -> Result<Submenu<Wry>, tauri::Error> {
    // Snapshot everything needed under ONE config read lock, then drop it BEFORE
    // any muda calls (menu construction dispatches to the main thread; never
    // hold the config lock across that).
    let (custom_count, seen, jack_names, effective): (
        usize,
        Vec<String>,                              // seen client names
        Vec<String>,                              // global jack names
        Vec<(String, bool, Vec<(String, bool)>)>, // per client: (name, custom_enabled, [(jack, effective)])
    ) = {
        let cfg = state.config.read();
        let seen: Vec<String> = cfg.seen_clients.iter().map(|c| c.name.clone()).collect();
        let jack_names: Vec<String> = cfg.jacks.iter().map(|j| j.name.clone()).collect();
        let effective: Vec<(String, bool, Vec<(String, bool)>)> = seen
            .iter()
            .map(|client| {
                // Authoritative Custom-enabled flag for this client (read fresh
                // every build), driving the new "Enable Custom permissions"
                // checkbox's checked state.
                let custom_enabled = cfg
                    .client_overrides
                    .get(client)
                    .map(|o| o.enabled)
                    .unwrap_or(false);
                let per_jack: Vec<(String, bool)> = jack_names
                    .iter()
                    .map(|jack| {
                        (jack.clone(), cfg.effective_patched(jack.as_str(), Some(client.as_str())))
                    })
                    .collect();
                (client.clone(), custom_enabled, per_jack)
            })
            .collect();
        (cfg.custom_client_count(), seen, jack_names, effective)
    };

    let mut custom_entries: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();

    if seen.is_empty() {
        // Placeholder so the submenu isn't empty (an agent appears here after it
        // first connects — recorded by AppState::record_seen_client). Uses an
        // opaque id (disabled, so it's never clickable).
        let pid = next_opaque_id(app);
        let ph = MenuItem::with_id(app, pid.as_str(), "(no agents seen yet)", false, None::<&str>)?;
        custom_entries.push(Box::new(ph));
    } else {
        for (client_name, custom_enabled, per_jack) in &effective {
            let mut per_client: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();

            // (S11) Explicit "Enable Custom permissions" checkbox as the FIRST
            // entry, before a separator + the per-jack list. Makes "which
            // agents get Custom mode" a deliberate action instead of an implicit
            // side effect of touching the first jack checkbox.
            {
                let id = next_enable_custom_id(app);
                let item = CheckMenuItem::with_id(
                    app,
                    id.as_str(),
                    "Enable Custom permissions",
                    true,
                    *custom_enabled,
                    None::<&str>,
                )?;
                if let Some(items) = app.try_state::<TrayItems>() {
                    items.enable_custom.lock().insert(
                        id,
                        EnableCustomItem {
                            item: item.clone(),
                            client: client_name.clone(),
                        },
                    );
                }
                per_client.push(Box::new(item));
            }

            // Separator between the Enable toggle and the per-jack list.
            per_client.push(Box::new(PredefinedMenuItem::separator(app)?));

            if jack_names.is_empty() {
                let pid = next_opaque_id(app);
                let ph = MenuItem::with_id(
                    app,
                    pid.as_str(),
                    "(no servers configured)",
                    false,
                    None::<&str>,
                )?;
                per_client.push(Box::new(ph));
            } else {
                for (jack_name, checked) in per_jack {
                    let id = next_opaque_id(app);
                    let item = CheckMenuItem::with_id(
                        app,
                        id.as_str(),
                        jack_name.as_str(),
                        true,
                        *checked,
                        None::<&str>,
                    )?;
                    if let Some(items) = app.try_state::<TrayItems>() {
                        items.overrides.lock().insert(
                            id,
                            OverrideItem {
                                item: item.clone(),
                                client: client_name.clone(),
                                jack: jack_name.clone(),
                            },
                        );
                    }
                    per_client.push(Box::new(item));
                }
            }

            // (S12) Separator + "✕ Delete this agent" action at the END of this
            // agent's own submenu (after the Enable checkbox, its separator, and
            // the per-jack list). Lives ONLY in Custom (NOT Forbidden): a plain
            // MenuItem (not a checkbox) because it's a one-shot destructive
            // action (purge the identity from seen_clients + client_overrides +
            // forbidden_clients), not a state toggle — clicking shows a blocking
            // native confirm dialog before doing anything. Keyed by an opaque
            // "delete_client:<n>" id in TrayItems::delete_client so the identity
            // is resolved from the lookup table, never encoded in the id.
            per_client.push(Box::new(PredefinedMenuItem::separator(app)?));
            {
                let id = next_delete_client_id(app);
                let item = MenuItem::with_id(
                    app,
                    id.as_str(),
                    "\u{2715} Delete this agent",
                    true,
                    None::<&str>,
                )?;
                if let Some(items) = app.try_state::<TrayItems>() {
                    items.delete_client.lock().insert(
                        id,
                        DeleteClientItem {
                            identity: client_name.clone(),
                        },
                    );
                }
                per_client.push(Box::new(item));
            }

            let per_child_refs: Vec<&dyn IsMenuItem<Wry>> =
                per_client.iter().map(|b| b.as_ref()).collect();
            let client_submenu = Submenu::with_items(app, client_name.as_str(), true, &per_child_refs)?;
            custom_entries.push(Box::new(client_submenu));
        }
    }

    // Submenu::with_items takes &[&dyn IsMenuItem]; we hold Box<dyn> so build a
    // slice of references.
    let refs: Vec<&dyn IsMenuItem<Wry>> = custom_entries.iter().map(|b| b.as_ref()).collect();
    // [enabled/total]: how many of the known agents currently have Custom
    // permissions ENABLED, out of how many are known at all.
    let label = format!("Custom [{}/{}]", custom_count, seen.len());
    Submenu::with_items(app, label.as_str(), true, &refs)
}

/// Mint the next opaque `"ovr:<n>"` menu id (S10), bumping the per-build counter
/// in [`TrayItems`] so every generated id — real override checkbox OR disabled
/// placeholder — is unique without embedding any external string.
fn next_opaque_id(app: &AppHandle) -> String {
    let n = match app.try_state::<TrayItems>() {
        Some(items) => {
            let mut c = items.override_counter.lock();
            *c += 1;
            *c
        }
        None => {
            // No managed TrayItems (shouldn't happen in normal builds): fall back
            // to a uuid-ish timestamp so the id is still unique enough.
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        }
    };
    format!("ovr:{}", n)
}

/// Mint the next opaque `"enable_custom:<n>"` menu id (S11), bumping the
/// per-build counter in [`TrayItems`] so every per-client "Enable Custom
/// permissions" checkbox id is unique within a build without embedding the
/// external client name.
fn next_enable_custom_id(app: &AppHandle) -> String {
    let n = match app.try_state::<TrayItems>() {
        Some(items) => {
            let mut c = items.enable_custom_counter.lock();
            *c += 1;
            *c
        }
        None => {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        }
    };
    format!("enable_custom:{}", n)
}

/// Mint the next opaque `"delete_client:<n>"` menu id (S12), bumping the
/// per-build counter in [`TrayItems`] so every per-agent "✕ Delete this agent"
/// item id is unique within a build without embedding the external identity.
fn next_delete_client_id(app: &AppHandle) -> String {
    let n = match app.try_state::<TrayItems>() {
        Some(items) => {
            let mut c = items.delete_client_counter.lock();
            *c += 1;
            *c
        }
        None => {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        }
    };
    format!("delete_client:{}", n)
}

/// Build the S11 "Forbidden (N)" submenu: one `CheckMenuItem` per EVERY seen
/// client identity (not just currently-forbidden ones), so Forbidden is a
/// symmetric on/off control over every known agent. The checkbox mirrors
/// `is_forbidden` (checked = currently denied) and the label reads
/// `"<name> [denied]"` / `"<name> [allowed]"`. Toggling drives
/// `AppState::set_forbidden`. N = `forbidden_clients.len()` (currently-denied
/// count, NOT seen_clients count). Uses OPAQUE `"forbid:<n>"` ids registered in
/// [`TrayItems::forbidden`] (mapping to a [`ForbiddenItem`] carrying the handle
/// + identity), so the arbitrary identity string never has to be encoded into an
/// id and the item can be reconciled (`set_checked` + `set_text`) after a click.
fn build_forbidden_submenu(app: &AppHandle, state: &AppState) -> Result<Submenu<Wry>, tauri::Error> {
    // Snapshot every seen identity + its current forbidden state under one
    // config read, then drop it BEFORE any muda calls (never hold the config
    // lock across menu construction).
    let (identities, states): (Vec<String>, Vec<bool>) = {
        let cfg = state.config.read();
        let identities: Vec<String> = cfg.seen_clients.iter().map(|c| c.name.clone()).collect();
        let states: Vec<bool> = identities
            .iter()
            .map(|name| cfg.is_forbidden(Some(name.as_str())))
            .collect();
        (identities, states)
    };
    // The "(N)" count is the currently-denied count (unchanged from S10c),
    // NOT the number of seen_clients.
    let denied_count = states.iter().filter(|b| **b).count();

    let mut entries: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();
    if identities.is_empty() {
        // Placeholder (disabled, never clickable) so the submenu isn't empty —
        // shown until at least one agent has connected.
        let pid = next_forbidden_id(app);
        let ph = MenuItem::with_id(app, pid.as_str(), "(no agents seen yet)", false, None::<&str>)?;
        entries.push(Box::new(ph));
    } else {
        for (identity, denied) in identities.iter().zip(states.iter()) {
            let id = next_forbidden_id(app);
            let label = forbidden_item_label(identity, *denied);
            let item = CheckMenuItem::with_id(app, id.as_str(), label.as_str(), true, *denied, None::<&str>)?;
            if let Some(items) = app.try_state::<TrayItems>() {
                items.forbidden.lock().insert(
                    id,
                    ForbiddenItem {
                        item: item.clone(),
                        identity: identity.clone(),
                    },
                );
            }
            entries.push(Box::new(item));
        }
    }

    let refs: Vec<&dyn IsMenuItem<Wry>> = entries.iter().map(|b| b.as_ref()).collect();
    let label = format!("Forbidden [{}]", denied_count);
    Submenu::with_items(app, label.as_str(), true, &refs)
}

/// Label for a Forbidden-submenu toggle checkbox: `"<name> [denied]"` when the
/// identity is currently forbidden, `"<name> [allowed]"` otherwise. Kept as a
/// helper so both the build path and the click-reconcile path produce the SAME
/// text from the SAME authoritative state.
fn forbidden_item_label(identity: &str, forbidden: bool) -> String {
    if forbidden {
        format!("{} [denied]", identity)
    } else {
        format!("{} [allowed]", identity)
    }
}

/// Mint the next opaque `"forbid:<n>"` menu id (S10c/S11), bumping the per-build
/// counter in [`TrayItems`] so every forbidden-toggle id (real checkbox OR
/// disabled placeholder) is unique without embedding the identity string.
fn next_forbidden_id(app: &AppHandle) -> String {
    let n = match app.try_state::<TrayItems>() {
        Some(items) => {
            let mut c = items.forbidden_counter.lock();
            *c += 1;
            *c
        }
        None => {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        }
    };
    format!("forbid:{}", n)
}

/// The one menu-event dispatcher installed on the tray icon (`on_menu_event`).
pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id.as_ref();
    if let Some(name) = id.strip_prefix("jack:") {
        on_jack_click(app, name);
        return;
    }
    // (S10) Per-client-per-jack override checkboxes use an OPAQUE id; resolve
    // the meaning from the overrides lookup table, not by parsing the id.
    if id.starts_with("ovr:") {
        on_override_click(app, id);
        return;
    }
    // (S11) Per-client "Enable Custom permissions" checkboxes use an OPAQUE id;
    // resolve the client name from the enable_custom lookup table.
    if id.starts_with("enable_custom:") {
        on_enable_custom_click(app, id);
        return;
    }
    // (S10c/S11) "Forbidden" per-identity toggle checkboxes use an OPAQUE id;
    // resolve the identity from the forbidden lookup table.
    if id.starts_with("forbid:") {
        on_forbidden_toggle_click(app, id);
        return;
    }
    // (S12) "✕ Delete this agent" items use an OPAQUE id; resolve the identity
    // from the delete_client lookup table.
    if id.starts_with("delete_client:") {
        on_delete_client_click(app, id);
        return;
    }
    match id {
        ID_RETRY_GATEWAY => on_retry_gateway(app),
        ID_RELOAD => on_reload(app),
        ID_OPEN_CONFIG => on_open_config(app),
        ID_OPEN_LOGS => on_open_logs(app),
        ID_COPY_URL => on_copy_url(app),
        ID_AUTOSTART => on_autostart_click(app),
        ID_REQUIRE_APPROVAL => on_require_approval_click(app),
        ID_REQUEST_LOGGING => on_request_logging_click(app),
        ID_ABOUT => on_about(app),
        ID_QUIT => on_quit(app),
        _ => {}
    }
}

/// Tooltip text reflecting the gateway lifecycle (a `Failed` bind takes
/// priority), then the live patched/total counts + gateway URL.
pub fn tooltip_text(app: &AppHandle) -> String {
    let Some(state) = app_state(app) else {
        return "Patchbay".to_string();
    };
    // Port-conflict / bind failure takes priority and is surfaced in the
    // tooltip; the "Retry gateway" tray item re-attempts the bind (S7).
    if let Some(err) = state.config_error.read().as_ref() {
        return format!(
            "Patchbay — CONFIG ERROR: {} (fix patchbay.json, then Reload config)",
            err
        );
    }
    if let GatewayStatus::Failed { reason } = &*state.status.read() {
        return format!("Patchbay — GATEWAY FAILED: {}", reason);
    }
    let lines = state.jack_lines();
    let patched = lines.iter().filter(|l| l.patched).count();
    let total = lines.len();
    let port = state.config.read().port;
    if total == 0 {
        format!(
            "Patchbay — no servers configured · http://127.0.0.1:{}/mcp",
            port
        )
    } else {
        format!(
            "Patchbay — {}/{} patched · http://127.0.0.1:{}/mcp",
            patched, total, port
        )
    }
}

// ---- click handlers -----------------------------------------------------

/// A jack's `CheckMenuItem` was clicked. Read the authoritative `patched`,
/// drive the opposite through `set_patched`, then reconcile the box + tooltip.
fn on_jack_click(app: &AppHandle, name: &str) {
    let Some(state) = app_state(app) else {
        return;
    };
    // AUTHORITATIVE current patched flag (muda already flipped the menu item).
    let current_patched = {
        let cfg = state.config.read();
        cfg.jacks
            .iter()
            .find(|j| j.name == name)
            .map(|j| j.patched)
            .unwrap_or(false)
    };
    let want = !current_patched;

    let app2 = app.clone();
    let name_owned = name.to_string();
    let state2 = state.clone();
    tauri::async_runtime::spawn(async move {
        let result: ToggleResult = state2.set_patched(&name_owned, want).await;
        log(&format!(
            "tray: toggle '{}' -> patched={} status={}",
            name_owned, result.patched, result.status
        ));
        // Reconcile the check box with the authoritative flag (a failed ON
        // keeps patched=true so the box stays checked; tooltip shows failure).
        set_check(&app2, &name_owned, result.patched);
        refresh_tooltip(&app2);
    });
}

/// A per-client-per-jack override checkbox was clicked (S10). Same "intent is
/// not state" discipline as `on_jack_click`: muda already flipped the box, so
/// read the AUTHORITATIVE effective value, drive the opposite through
/// `set_client_override`, then reconcile `set_checked`. Toggling any jack under
/// a client also lazily activates Custom mode for it on first use.
fn on_override_click(app: &AppHandle, id: &str) {
    let Some(state) = app_state(app) else {
        return;
    };
    // Resolve the (client, jack) meaning from the overrides lookup table.
    let (client, jack) = {
        let Some(items) = app.try_state::<TrayItems>() else {
            return;
        };
        let ovr = match items.overrides.lock().get(id).cloned() {
            Some(o) => o,
            None => {
                log(&format!("tray: override click on unknown id '{}' (stale menu?)", id));
                return;
            }
        };
        (ovr.client, ovr.jack)
    };

    // AUTHORITATIVE current effective value (muda already flipped the box).
    let current = {
        let cfg = state.config.read();
        cfg.effective_patched(&jack, Some(client.as_str()))
    };
    let want = !current;

    // Was this client already in Custom mode? If toggling here will NEWLY enable
    // Custom for it, the "Custom (N)" count changes -> rebuild the whole menu so
    // the label stays correct. Otherwise just reconcile this one checkbox.
    let was_custom = {
        let cfg = state.config.read();
        cfg.client_overrides
            .get(&client)
            .map(|o| o.enabled)
            .unwrap_or(false)
    };

    let app2 = app.clone();
    let id_owned = id.to_string();
    let state2 = state.clone();
    tauri::async_runtime::spawn(async move {
        log(&format!(
            "tray: override '{}'/'{}' -> want={} (was_custom={})",
            client, jack, want, was_custom
        ));
        match state2.set_client_override(&client, &jack, want).await {
            Ok(effective) => {
                if !was_custom {
                    // Count increased (Custom newly enabled): rebuild the whole
                    // menu so the "Custom (N)" label + every checkbox reflects
                    // authoritative state.
                    rebuild_menu_and_refresh(&app2);
                } else {
                    // Reconcile just this checkbox with the resulting value.
                    set_override_check_by_id(&app2, &id_owned, effective);
                    refresh_tooltip(&app2);
                }
            }
            Err(e) => {
                log(&format!("tray: set_client_override failed: {}", e));
                // Reconcile back to the authoritative value so the box doesn't
                // lie about a state we couldn't persist.
                let effective = state2
                    .config
                    .read()
                    .effective_patched(&jack, Some(client.as_str()));
                set_override_check_by_id(&app2, &id_owned, effective);
                refresh_tooltip(&app2);
            }
        }
    });
}

/// "Retry gateway": re-spawn [`gateway::run_gateway`] for a fresh bind attempt
/// (e.g. after a port conflict cleared), then refresh the tooltip so the
/// Running/Failed result is reflected (S7). `run_gateway` serves forever on
/// success, so it is spawned detached rather than awaited here.
fn on_retry_gateway(app: &AppHandle) {
    let Some(state) = app_state(app) else {
        return;
    };
    let port = state.config.read().port;
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        log(&format!("tray: retry gateway on port {}", port));
        // (FIX 6) If the gateway is already Running, re-spawning run_gateway
        // would try to re-bind against our own live listener and wrongly flip
        // the status to Failed. Just refresh the tooltip and return.
        let already_running = matches!(*state.status.read(), GatewayStatus::Running { .. });
        if already_running {
            refresh_tooltip(&app2);
            return;
        }
        // Detached: a successful bind serves until shutdown, so awaiting it
        // would block the tooltip refresh forever.
        let gw_state = state.clone();
        tauri::async_runtime::spawn(async move {
            gateway::run_gateway(gw_state, port).await;
        });
        // Give the fresh bind a moment to succeed or fail, then reflect it.
        tokio::time::sleep(Duration::from_millis(750)).await;
        refresh_tooltip(&app2);
    });
}

/// "Reload config": re-read from disk, diff jacks (start newly-patched, stop
/// removed/unpatched), broadcast, then rebuild the whole menu via `set_menu`.
fn on_reload(app: &AppHandle) {
    let Some(state) = app_state(app) else {
        return;
    };
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        log("tray: reload config requested");
        // (FIX 4) Use load_result so a corrupt config NEVER wipes the live
        // in-memory config (and is never overwritten). Keep the current config
        // on a parse/IO error and surface the reason in the tooltip.
        match config::load_result() {
            Ok(new_cfg) => {
                // The file parsed cleanly: clear any prior config error.
                *state.config_error.write() = None;

                let old = state.config.read().clone();
                let port_changed = new_cfg.port != old.port;
                let new_port = new_cfg.port;
                // Replace the in-memory config with the freshly-read disk copy.
                *state.config.write() = new_cfg.clone();

                // Reconcile upstreams against should_run (S10): GLOBAL OR any
                // enabled Custom client. A jack whose child should now run but
                // isn't -> start; one that should no longer run but is -> stop.
                for jack in &new_cfg.jacks {
                    let should_run = new_cfg.should_run_jack(&jack.name);
                    let is_running = state.upstream.is_jack_running(&jack.name);
                    if should_run && !is_running {
                        let _g = state.upstream.jack_lock(&jack.name).await;
                        state
                            .upstream
                            .start_jack(jack, state.sessions.clone(), state.config.clone())
                            .await;
                    } else if !should_run && is_running {
                        let _g = state.upstream.jack_lock(&jack.name).await;
                        state.upstream.stop_jack(&jack.name).await;
                    }
                }
                // Stop jacks removed from the config entirely.
                for old_jack in &old.jacks {
                    if !new_cfg.jacks.iter().any(|j| j.name == old_jack.name) {
                        let _g = state.upstream.jack_lock(&old_jack.name).await;
                        state.upstream.stop_jack(&old_jack.name).await;
                    }
                }

                state.sessions.broadcast_tools_list_changed().await;

                if port_changed {
                    log(&format!(
                        "tray: gateway port changed {} -> {}, rebinding listener",
                        old.port, new_port
                    ));
                    // End long-lived SSE responses before asking axum to stop;
                    // otherwise graceful shutdown can wait forever on clients
                    // connected to the old listener.
                    state.sessions.close_all_streams();
                    state.shutdown_gateway.notify_waiters();

                    let gw_state = state.clone();
                    tauri::async_runtime::spawn(async move {
                        gateway::run_gateway(gw_state, new_port).await;
                    });

                    // Give the fresh bind a moment to settle so the final
                    // tooltip refresh reflects Running/Failed on the new port.
                    tokio::time::sleep(Duration::from_millis(750)).await;
                    crate::utils::request_log::log_event(&state, "gateway_port_rebind");
                }
            }
            Err(config::ConfigError::Missing) => {
                log("tray: reload skipped — config file missing, keeping current config");
            }
            Err(e) => {
                let reason = e.reason();
                log(&format!(
                    "tray: reload FAILED to parse config ({}), keeping current config",
                    reason
                ));
                *state.config_error.write() = Some(reason);
            }
        }

        // Rebuild the whole menu + refresh the tooltip.
        let _ = rebuild_menu(&app2);
        refresh_tooltip(&app2);
        log("tray: reload config complete");
        crate::utils::request_log::log_event(&state, "reload_config");
    });
}

/// "Open config file": open `patchbay.json` for editing.
fn on_open_config(_app: &AppHandle) {
    let path = config::config_file_path();
    log(&format!("tray: opening config file {}", path.display()));
    open_text_file(&path, "config");
}

/// "Open logs folder": reveal `%APPDATA%\Patchbay\logs` in Windows Explorer so
/// the user can browse both the Level-1 `patchbay_rCURRENT.log` and the
/// Level-2 `requests/` subdir. Launches `explorer.exe <path>` DIRECTLY (not via
/// `cmd /C start`) — the same direct-launch style as [`open_text_file`], which
/// sidesteps any broken folder-association the OS might have.
fn on_open_logs(_app: &AppHandle) {
    let dir = crate::utils::log::logs_dir();
    log(&format!("tray: opening logs folder {}", dir.display()));
    open_folder_in_explorer(&dir);
}

/// "About": write the embedded [`ABOUT_TEXT`] to `About.txt` next to
/// `patchbay.json` (overwriting it fresh on every click, so it always matches
/// the running exe's text) then open it.
fn on_about(_app: &AppHandle) {
    let path = config::config_dir().join("About.txt");
    if let Err(e) = std::fs::write(&path, ABOUT_TEXT) {
        log(&format!("tray: failed to write About.txt: {e}"));
        return;
    }
    log(&format!("tray: opening about file {}", path.display()));
    open_text_file(&path, "about");
}

/// Open a plain-text file for the user to read/edit. Launches `notepad.exe`
/// DIRECTLY (by executable name) rather than going through the OS's default
/// file-association handler (`cmd /C start "" "<path>"`): on some Windows
/// installs the `.txt`/`.json` association is broken (points at a stale or
/// removed app, or — as observed live — at the Store's "App Installer",
/// producing a bogus "package could not be installed" dialog on open).
/// Bypassing the association with a known-present executable sidesteps that
/// entirely. Falls back to the association-based `start`, then to revealing
/// the file in Explorer, if `notepad.exe` itself can't be spawned.
fn open_text_file(path: &std::path::Path, log_tag: &str) {
    let path_str = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Err(e) = std::process::Command::new("notepad.exe")
            .arg(&path_str)
            .spawn()
        {
            log(&format!(
                "tray: open {log_tag} via notepad.exe failed ({e}); trying default handler"
            ));
            let opened = std::process::Command::new("cmd")
                .args(["/C", "start", "", &path_str])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
            if let Err(e) = opened {
                log(&format!(
                    "tray: open {log_tag} via start failed ({e}); revealing in Explorer"
                ));
                let _ = std::process::Command::new("explorer")
                    .arg(format!("/select,{path_str}"))
                    .spawn();
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(&path_str).spawn();
    }
}

/// Open a FOLDER in the OS file manager. On Windows, launches `explorer.exe
/// <path>` directly (NOT `cmd /C start ""` — see [`open_text_file`] for why the
/// association-based path is avoided). No fallback chain: if Explorer can't be
/// spawned there's nothing better to try for a directory.
fn open_folder_in_explorer(path: &std::path::Path) {
    let path_str = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("explorer")
            .arg(&path_str)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(&path_str).spawn();
    }
}

/// "Copy gateway URL": put `http://127.0.0.1:<port>/mcp` on the OS clipboard so
/// the user can paste it into an agent's config. Uses `arboard` (works without
/// a window); logs the URL on failure so it's still recoverable from the log.
fn on_copy_url(app: &AppHandle) {
    let Some(state) = app_state(app) else {
        return;
    };
    let port = state.config.read().port;
    let url = format!("http://127.0.0.1:{}/mcp", port);
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(url.clone())) {
        Ok(()) => log(&format!("tray: copied gateway URL to clipboard: {url}")),
        Err(e) => log(&format!("tray: clipboard copy failed ({e}); gateway URL = {url}")),
    }
    crate::utils::request_log::log_event(&state, "copy_gateway_url");
}

/// "Start with Windows": flip + persist config.autostart, then drive the
/// registry, then reconcile the check box with the resulting flag.
fn on_autostart_click(app: &AppHandle) {
    let Some(state) = app_state(app) else {
        return;
    };
    let current = state.config.read().autostart;
    let want = !current;

    {
        let mut cfg = state.config.write();
        cfg.autostart = want;
        let snap = cfg.clone();
        drop(cfg);
        if let Err(e) = config::save(&snap) {
            log(&format!("tray: failed to persist autostart: {}", e));
        }
    }

    if let Err(e) = autorun::set_autorun(want) {
        log(&format!("tray: set_autorun({}) failed: {}", want, e));
    }

    let actual = state.config.read().autostart;
    set_check(app, ID_AUTOSTART, actual);
}

/// "Require approval for new agents" (S10c): flip + persist
/// `config.require_approval_for_new_clients`, then reconcile the check box. No
/// other side effects — flipping it OFF simply means new/unknown clients skip
/// the dialog and are auto-added to `seen_clients` (today's S10 behavior); ON
/// means a never-seen identity pops the approval dialog on first connect.
fn on_require_approval_click(app: &AppHandle) {
    let Some(state) = app_state(app) else {
        return;
    };
    let current = state.config.read().require_approval_for_new_clients;
    let want = !current;

    {
        let mut cfg = state.config.write();
        cfg.require_approval_for_new_clients = want;
        let snap = cfg.clone();
        drop(cfg);
        if let Err(e) = config::save(&snap) {
            log(&format!("tray: failed to persist require_approval: {}", e));
        }
    }

    let actual = state.config.read().require_approval_for_new_clients;
    set_check(app, ID_REQUIRE_APPROVAL, actual);
}

/// "Enable request logging" (Level-2): read the AUTHORITATIVE current flag,
/// flip it, persist via the save-then-commit [`AppState::set_request_logging_enabled`]
/// setter, then reconcile the box with the AUTHORITATIVE resulting value (so a
/// persist failure rolls the box back to the unchanged flag). The toggle event
/// itself is recorded to the request log when it ends up ON.
fn on_request_logging_click(app: &AppHandle) {
    let Some(state) = app_state(app) else {
        return;
    };
    let current = state.config.read().request_logging_enabled;
    let want = !current;

    if let Err(e) = state.set_request_logging_enabled(want) {
        log(&format!("tray: failed to persist request_logging: {}", e));
    }

    let actual = state.config.read().request_logging_enabled;
    set_check(app, ID_REQUEST_LOGGING, actual);
    crate::utils::request_log::log_event(
        &state,
        &format!("request_logging -> enabled={}", actual),
    );
}

/// A "Forbidden" per-identity toggle checkbox was clicked (S11). Same "intent
/// is not state" discipline as [`on_jack_click`]/[`on_override_click`]: muda
/// already flipped the box, so read the AUTHORITATIVE `is_forbidden` value from
/// config, compute the desired opposite, drive it through
/// `AppState::set_forbidden`, then reconcile the item's checked state AND its
/// text label (`"<name> [denied]"` ↔ `"<name> [allowed]"`) with the resulting
/// authoritative state. Finally a full menu rebuild refreshes the "Forbidden
/// (N)" / "Custom (N)" counts live.
fn on_forbidden_toggle_click(app: &AppHandle, id: &str) {
    let Some(state) = app_state(app) else {
        return;
    };
    // Resolve the identity (and item handle) from the forbidden lookup table.
    let (identity, item) = {
        let Some(items) = app.try_state::<TrayItems>() else {
            return;
        };
        let f = match items.forbidden.lock().get(id).cloned() {
            Some(f) => f,
            None => {
                log(&format!(
                    "tray: forbidden toggle click on unknown id '{}' (stale menu?)",
                    id
                ));
                return;
            }
        };
        (f.identity, f.item)
    };

    // AUTHORITATIVE current forbidden state (muda already flipped the box).
    let current = state.config.read().is_forbidden(Some(identity.as_str()));
    let want = !current;

    let app2 = app.clone();
    let state2 = state.clone();
    tauri::async_runtime::spawn(async move {
        log(&format!("tray: forbidden toggle '{}' -> want={}", identity, want));
        match state2.set_forbidden(&identity, want).await {
            Ok(()) => {
                // Reconcile the clicked item's check + label with the resulting
                // authoritative state (immediate feedback even before the
                // count-driven rebuild below runs).
                let resulting = state2.config.read().is_forbidden(Some(identity.as_str()));
                reconcile_forbidden_item(&item, &identity, resulting);
            }
            Err(e) => {
                log(&format!("tray: set_forbidden failed: {}", e));
                // Reconcile back to the authoritative value so the box/label
                // don't lie about a state we couldn't persist.
                let resulting = state2.config.read().is_forbidden(Some(identity.as_str()));
                reconcile_forbidden_item(&item, &identity, resulting);
            }
        }
        // Rebuild the whole menu so the "Forbidden (N)" / "Custom (N)" counts
        // reflect the change (a forbidden toggle can change BOTH counts).
        rebuild_menu_and_refresh(&app2);
    });
}

/// A per-client "Enable Custom permissions" checkbox was clicked (S11). Same
/// "intent is not state" discipline: read the AUTHORITATIVE `enabled` flag from
/// config, compute the desired opposite, drive it through
/// `enable_custom_client` (ON) / `disable_custom_client` (OFF), then rebuild the
/// whole menu — the "Custom (N)" count changes on every real toggle, so a full
/// rebuild (regenerating every checkbox + label from authoritative config) is
/// the cleanest reconciliation.
fn on_enable_custom_click(app: &AppHandle, id: &str) {
    let Some(state) = app_state(app) else {
        return;
    };
    // Resolve the client name from the enable_custom lookup table.
    let client = {
        let Some(items) = app.try_state::<TrayItems>() else {
            return;
        };
        let found = items.enable_custom.lock().get(id).cloned();
        match found {
            Some(e) => e.client,
            None => {
                log(&format!(
                    "tray: enable_custom click on unknown id '{}' (stale menu?)",
                    id
                ));
                return;
            }
        }
    };

    // AUTHORITATIVE current Custom-enabled state (muda already flipped the box).
    let current = state
        .config
        .read()
        .client_overrides
        .get(&client)
        .map(|o| o.enabled)
        .unwrap_or(false);
    let want = !current;

    let app2 = app.clone();
    let state2 = state.clone();
    tauri::async_runtime::spawn(async move {
        log(&format!("tray: enable_custom '{}' -> want={}", client, want));
        let res = if want {
            state2.enable_custom_client(&client).await
        } else {
            state2.disable_custom_client(&client).await
        };
        if let Err(e) = res {
            log(&format!("tray: enable/disable_custom_client failed: {}", e));
        }
        // Rebuild the whole menu so "Custom (N)" + every per-jack checkbox (whose
        // effective value can flip between the Custom list and global when Custom
        // toggles) reflect the authoritative state.
        rebuild_menu_and_refresh(&app2);
    });
}

/// A "✕ Delete this agent" item was clicked (S12). Shows a blocking native
/// confirmation dialog (because permanently deleting an identity is
/// irreversible), then on explicit confirmation purges the identity from
/// `seen_clients` + `client_overrides` + `forbidden_clients` via
/// [`AppState::delete_client`] and rebuilds the whole menu so the deleted agent
/// disappears from BOTH the Custom and Forbidden submenus and both "(N)" counts
/// update. On cancel/no: does nothing.
///
/// The confirm dialog is a BLOCKING `MessageBoxW` running its own modal message
/// loop, so — exactly like [`AppState::ensure_client_approved`] does for the
/// first-connection approval dialog — it runs on a plain `std::thread::spawn`,
/// NOT on the tauri main/event thread (which would freeze the tray while the
/// dialog is up) and NOT on the tokio runtime. The async hand-off
/// (`delete_client` + full menu rebuild) happens inside
/// `tauri::async_runtime::spawn` once the dialog returns.
fn on_delete_client_click(app: &AppHandle, id: &str) {
    let Some(state) = app_state(app) else {
        return;
    };
    // Resolve the identity from the delete_client lookup table (the arbitrary
    // external `clientInfo.name` is never encoded into the opaque id).
    let identity = {
        let Some(items) = app.try_state::<TrayItems>() else {
            return;
        };
        let d = match items.delete_client.lock().get(id).cloned() {
            Some(d) => d,
            None => {
                log(&format!(
                    "tray: delete_client click on unknown id '{}' (stale menu?)",
                    id
                ));
                return;
            }
        };
        d.identity
    };

    let app2 = app.clone();
    let state2 = state.clone();
    std::thread::spawn(move || {
        // Blocking Win32 modal — runs on this dedicated std thread so the
        // tray/event thread is never frozen waiting on the user to answer.
        let confirmed = crate::approval::show_delete_confirm_dialog(&identity);
        if !confirmed {
            log(&format!(
                "tray: delete '{}' cancelled by user (no changes made)",
                identity
            ));
            return;
        }
        // Confirmed — hand off to the async side for the actual deletion + tray
        // rebuild. delete_client runs the save-then-commit discipline +
        // reconcile_all_jack_lifecycles (removing a Custom override could have
        // been the only thing keeping some globally-off jack's shared child
        // alive — caught the same way set_forbidden/disable_custom_client do).
        tauri::async_runtime::spawn(async move {
            match state2.delete_client(&identity).await {
                Ok(()) => {
                    log(&format!("tray: deleted agent '{}' from Patchbay", identity));
                }
                Err(e) => {
                    log(&format!("tray: delete_client '{}' failed: {}", identity, e));
                }
            }
            // Rebuild the whole menu so the deleted agent is gone from BOTH the
            // Custom and Forbidden submenus and both "(N)" counts update.
            rebuild_menu_and_refresh(&app2);
        });
    });
}

/// "Quit": log + exit. The process-wide Job Object (KILL_ON_JOB_CLOSE) reaps
/// any stdio children on exit, so a hard exit leaves zero orphans.
fn on_quit(app: &AppHandle) {
    log("tray: Quit requested");
    // Record the stop to the Level-2 request log (synchronous write completes
    // before the process exits).
    if let Some(state) = app_state(app) {
        crate::utils::request_log::log_event(&state, "app_stop");
    }
    std::process::exit(0);
}

// ---- helpers ------------------------------------------------------------

/// Clone of the managed `AppState` (cheap — all fields are `Arc`), if present.
fn app_state(app: &AppHandle) -> Option<AppState> {
    app.try_state::<AppState>().map(|s| s.inner().clone())
}

/// Look up the main tray icon (set at startup with id [`TRAY_ID`]).
fn get_tray(app: &AppHandle) -> Option<TrayIcon<Wry>> {
    app.tray_by_id(&TrayIconId::new(TRAY_ID))
}

/// Rebuild the menu from the live config and swap it onto the tray icon.
fn rebuild_menu(app: &AppHandle) -> Result<(), tauri::Error> {
    let Some(state) = app_state(app) else {
        return Ok(());
    };
    let menu = build_menu(app, &state)?;
    if let Some(tray) = get_tray(app) {
        tray.set_menu(Some(menu))?;
    }
    Ok(())
}

/// Reconcile a `CheckMenuItem`'s checked state with the authoritative flag.
/// `key` is a jack name, or [`ID_AUTOSTART`].
///
/// The handle is cloned out of the map and the lock released BEFORE
/// `set_checked` (which dispatches to + blocks on the main thread) so the main
/// thread can never deadlock against this lock.
fn set_check(app: &AppHandle, key: &str, checked: bool) {
    let item = app
        .try_state::<TrayItems>()
        .and_then(|items| items.checks.lock().get(key).cloned());
    if let Some(item) = item {
        if let Err(e) = item.set_checked(checked) {
            log(&format!(
                "tray: set_checked({}, {}) failed: {}",
                key, checked, e
            ));
        }
    }
}

/// Reconcile a per-client override checkbox (S10) by its opaque menu id with the
/// authoritative value. Same clone-out-of-map-then-drop discipline as
/// [`set_check`] so the main-thread `set_checked` can't deadlock against the
/// overrides lock.
fn set_override_check_by_id(app: &AppHandle, id: &str, checked: bool) {
    let item = app
        .try_state::<TrayItems>()
        .and_then(|items| items.overrides.lock().get(id).map(|o| o.item.clone()));
    if let Some(item) = item {
        if let Err(e) = item.set_checked(checked) {
            log(&format!(
                "tray: set_checked(override {}, {}) failed: {}",
                id, checked, e
            ));
        }
    }
}

/// Reconcile a "Forbidden" toggle checkbox (S11) with the authoritative
/// forbidden state: BOTH its checked flag AND its text label
/// (`"<name> [denied]"` ↔ `"<name> [allowed]"`) reflect the state. Takes the
/// cloned `CheckMenuItem` handle + the identity directly (the caller already
/// pulled both out of the lookup table under the lock), so the main-thread
/// `set_checked`/`set_text` calls can't deadlock against the forbidden lock and
/// the label is rebuilt from the known identity + new state (no fragile
/// re-parsing of the stored text).
fn reconcile_forbidden_item(item: &CheckMenuItem<Wry>, identity: &str, forbidden: bool) {
    if let Err(e) = item.set_checked(forbidden) {
        log(&format!(
            "tray: set_checked(forbidden {} {}, {}) failed: {}",
            identity,
            item.id().as_ref(),
            forbidden,
            e
        ));
    }
    let label = forbidden_item_label(identity, forbidden);
    if let Err(e) = item.set_text(label.as_str()) {
        log(&format!(
            "tray: set_text(forbidden {} {}, {}) failed: {}",
            identity,
            item.id().as_ref(),
            label,
            e
        ));
    }
}

/// Refresh the tray tooltip from the live config + gateway status. Public so
/// `main.rs` can call it shortly after startup to reflect the bind result.
pub fn refresh_tooltip(app: &AppHandle) {
    let tip = tooltip_text(app);
    if let Some(tray) = get_tray(app) {
        if let Err(e) = tray.set_tooltip(Some(tip)) {
            log(&format!("tray: set_tooltip failed: {}", e));
        }
    }
}

/// Rebuild the whole menu from the live config + refresh the tooltip. Public so
/// a background task (e.g. [`AppState::record_seen_client`] recording a
/// newly-seen MCP client from the gateway path, which has no `AppHandle` of its
/// own) can refresh the tray after a state change that happened off the
/// menu-event path. Reuses the SAME crossing mechanism (spawn + main-thread
/// `set_menu`) the other tray handlers use.
pub fn rebuild_menu_and_refresh(app: &AppHandle) {
    if let Err(e) = rebuild_menu(app) {
        log(&format!("tray: rebuild_menu failed: {}", e));
    }
    refresh_tooltip(app);
}
