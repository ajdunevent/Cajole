use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use cajole::model::{Alarm, OutputMode, RepeatDays, VibrationPattern};
use cajole::scheduler::{self, RingingMap, SharedTracker};
use cajole::{AlarmStore, TriggerTracker};
use chrono::{Datelike, NaiveDate, NaiveTime, Timelike, Weekday};
use uuid::Uuid;

const APP_ID: &str = cajole::APP_ID;

type RefreshFn = Rc<dyn Fn()>;

/// Asks the scheduler to recheck and reschedule its wake timer, after anything
/// that could change when it is next needed (an alarm saved, toggled, deleted
/// or dismissed early). Does nothing before the `SchedulerHandle` exists,
/// which avoids unwrapping an `Option` at every call site.
fn request_reschedule(scheduler_handle_cell: &Rc<RefCell<Option<scheduler::SchedulerHandle>>>) {
    let Some(handle) = scheduler_handle_cell.borrow().clone() else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        scheduler::check_and_reschedule(&handle).await;
    });
}
type RingDialogs = Rc<RefCell<HashMap<Uuid, adw::Window>>>;

/// Where alarms live on disk: `~/.local/share/cajole/alarms.json`, per the XDG
/// convention for application state (`~/.config` is for hand-edited settings).
fn store_path() -> PathBuf {
    let base = xdg_data_dir();
    base.join("cajole").join("alarms.json")
}

fn xdg_data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::Path::new(&home).join(".local").join("share")
}

fn sound_library_path() -> PathBuf {
    xdg_data_dir().join("cajole").join("sounds.json")
}

fn settings_path() -> PathBuf {
    xdg_data_dir().join("cajole").join("settings.json")
}

fn holiday_path() -> PathBuf {
    xdg_data_dir().join("cajole").join("holiday.json")
}

/// Where custom sounds are copied when added, in the app's directory so
/// moving, renaming or deleting the original cannot break an alarm.
fn custom_sounds_dir() -> PathBuf {
    xdg_data_dir().join("cajole").join("sounds")
}

/// Where downloaded default sounds are cached, separate from
/// `custom_sounds_dir` so "Reset Defaults" and per-entry removal can delete
/// from it without touching a person's own sounds.
fn downloaded_sounds_dir() -> PathBuf {
    xdg_data_dir().join("cajole").join("sounds").join("downloaded")
}

/// Plays `path` through the default audio output with `paplay` (shipped with
/// `pactl`, already required for `SinkControl`; it decodes FLAC and Ogg Vorbis
/// through libsndfile).
///
/// Not routed through feedbackd/`fbcli`, whose theme-event override does not
/// consistently honor `--sound-file`, and not handed to whatever app the
/// desktop associates with audio files.
///
/// Returns the running subprocess so the caller can kill it before starting
/// the next preview.
fn play_sound_preview(path: &str) -> anyhow::Result<gio::Subprocess> {
    gio::Subprocess::newv(
        &[std::ffi::OsStr::new("paplay"), std::ffi::OsStr::new(path)],
        gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .map_err(|err| anyhow::anyhow!("failed to spawn paplay: {err}"))
}

/// The Sound Manager's playing preview, shared across rows so only one plays
/// at a time. Unlike [`PreviewHandle`] (the alarm form's preview button), it
/// carries an `on_stop` callback: starting another row's preview must revert
/// the previous row's Stop icon to Play, not just kill its process.
struct ActivePreview {
    subprocess: gio::Subprocess,
    on_stop: Box<dyn Fn()>,
}

fn stop_active_preview(current: &Rc<RefCell<Option<ActivePreview>>>) {
    if let Some(active) = current.borrow_mut().take() {
        active.subprocess.force_exit();
        (active.on_stop)();
    }
}

/// A Sound Manager row's play and download state. `Downloading` applies only
/// to a not-yet-downloaded default whose Play was clicked (see
/// [`download_sound_async`]: Stop during this state does not cancel the
/// download, it skips the auto-play when the download finishes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowPlayState {
    Idle,
    Downloading,
    Playing,
}

/// Where the "not now" answer to the autostart prompt is remembered, so it is
/// not asked on every launch.
fn autostart_prompt_marker_path() -> PathBuf {
    xdg_data_dir()
        .join("cajole")
        .join("autostart_prompt_dismissed")
}

/// If Cajole is not set to start at login and the prompt was not dismissed
/// before, asks whether to turn that on, explaining what happens without it.
fn maybe_prompt_for_autostart(window: &adw::ApplicationWindow) {
    let marker = autostart_prompt_marker_path();

    if cajole::autostart::is_enabled() {
        return;
    }
    if cajole::autostart::prompt_dismissed_at(&marker) {
        return;
    }

    let dialog = adw::AlertDialog::builder()
        .heading("Start Cajole Automatically?")
        .body(
            "Cajole needs to be running in the background for your alarms to go off. If it \
             isn't started automatically when you log in, you'll need to open Cajole \
             yourself once after every login — otherwise your alarms won't ring.",
        )
        .build();
    dialog.add_response("not-now", "Not Now");
    dialog.add_response("enable", "Turn On");
    dialog.set_response_appearance("enable", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("enable"));
    dialog.set_close_response("not-now");

    dialog.connect_response(None, move |_, response| match response {
        "enable" => {
            glib::spawn_future_local(async {
                if let Err(err) = cajole::autostart::enable().await {
                    tracing::error!(%err, "failed to enable autostart");
                }
            });
        }
        _ => {
            if let Err(err) = cajole::autostart::dismiss_prompt_at(&marker) {
                tracing::warn!(%err, "failed to record autostart prompt dismissal");
            }
        }
    });

    dialog.present(Some(window));
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("cajole=info")),
        )
        .init();

    // `--hidden`: launched by the autostart entry at login, or by someone who
    // wants the scheduler running without a window. The main window is never
    // presented in this mode; see `build_ui`.
    //
    // `--gapplication-service` does not fit: it makes GIO skip `activate` at
    // startup and wait for an external trigger, but the scheduler must poll
    // immediately. `--hidden` still fires `activate` (starting the scheduler)
    // without showing a window.
    let hidden = std::env::args().any(|arg| arg == "--hidden");

    // Registers the bundled bytes (compiled by build.rs) under their resource
    // paths. Display-independent, so it runs here, unlike `add_resource_path`
    // below, which needs a Display and waits for the first `activate`. Icons
    // cannot be looked up until that second step.
    let resource_bytes = include_bytes!(concat!(env!("OUT_DIR"), "/cajole.gresource"));
    let resource = gio::Resource::from_data(&glib::Bytes::from_static(resource_bytes))
        .expect("failed to load bundled cajole.gresource — build.rs should have compiled this");
    gio::resources_register(&resource);

    // GTK takes the Wayland app ID from the program name, which defaults to
    // the binary name. Phosh logs "Could not find application for app-id
    // 'cajole'" otherwise.
    glib::set_prgname(Some(APP_ID));
    let app = adw::Application::builder().application_id(APP_ID).build();

    // `hidden` suppresses window presentation only on this process's first
    // activation (gated on `first_activation`). Every later activation,
    // forwarded over D-Bus when someone opens the app, is a request to see the
    // UI and always gets a window.
    //
    // `started` is a separate flag instead of checking for a window: in the
    // first-activation, hidden case no window exists either, for a different
    // reason.
    let started: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    app.connect_activate(move |app| {
        // `activate` can fire more than once: later launch attempts are
        // forwarded here over D-Bus (GApplication's single-instance behavior).
        // Only the first activation builds anything or takes the hold.
        let first_activation = !started.get();
        if first_activation {
            started.set(true);

            // Teaches this display's icon theme to resolve names under the
            // bundled badge icons' resource path. Needs a Display, so it waits
            // for the first activation and is gated by `first_activation` like
            // the rest.
            if let Some(display) = gtk4::gdk::Display::default() {
                gtk4::IconTheme::for_display(&display)
                    .add_resource_path("/net/dunevent/Cajole/icons");
            }

            // Keeps the app alive for the process lifetime. Without it,
            // GApplication quits when the last window closes, which would stop
            // the scheduler and every alarm. `hold()` returns a guard that
            // releases on drop; `forget` keeps it held.
            //
            // Called here in `activate`, gated by a `started` flag, not
            // unconditionally before `run()`: the earlier placement was meant
            // to avoid a race between `hold()` and GApplication's exit check
            // against a zero use-count. This placement is the pattern the
            // sibling project Ripples ships.
            std::mem::forget(app.hold());

            build_ui(app, hidden);
        }

        // Every activation except the first-and-hidden one shows the window.
        // `app.active_window()` is used instead of a separate
        // `Option<Window>`, since GTK tracks windows built with
        // `.application(app)`.
        if !(first_activation && hidden) {
            if let Some(window) = app.active_window() {
                window.present();
            }
        }
    });

    // Filtered from argv instead of built from scratch: GApplication needs a
    // normal-looking argument list (at least `argv[0]`) for D-Bus registration
    // and single-instance forwarding. An empty array avoided the "Unknown
    // option --hidden" rejection but fed GLib an unusual lifecycle; stripping
    // the one unrecognized flag keeps the rest intact.
    let filtered_args: Vec<String> = std::env::args().filter(|arg| arg != "--hidden").collect();
    app.run_with_args(&filtered_args)
}

fn build_ui(app: &adw::Application, hidden: bool) -> adw::ApplicationWindow {
    install_time_picker_css();
    let path = store_path();

    let ringing: RingingMap = Rc::new(RefCell::new(HashMap::new()));
    // Filled in once the `SchedulerHandle` is built below. It needs
    // `refresh_list`, and `refresh_list` must be usable by things defined
    // before that point; this cell breaks the cycle, as `RefreshCell` does for
    // the Sound Manager. Callers go through `request_reschedule`, which
    // handles the unset case.
    let scheduler_handle_cell: Rc<RefCell<Option<scheduler::SchedulerHandle>>> =
        Rc::new(RefCell::new(None));
    let tracker: SharedTracker = Rc::new(RefCell::new(TriggerTracker::new()));
    let open_ring_dialogs: RingDialogs = Rc::new(RefCell::new(HashMap::new()));

    let list_box = gtk4::ListBox::builder()
        .valign(gtk4::Align::Start)
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Holiday-only alarms get their own card, a separate `AdwClamp` below the
    // regular one with a title above it. `holiday_group` (title and list) is
    // hidden as one unit when empty.
    let holiday_list_box = gtk4::ListBox::builder()
        .valign(gtk4::Align::Start)
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let holiday_title = gtk4::Label::builder()
        .label("Holiday Alarms")
        .halign(gtk4::Align::Start)
        .margin_bottom(6)
        .margin_start(6)
        .css_classes(["heading"])
        .build();
    let holiday_group = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .margin_top(18)
        .margin_start(12)
        .margin_end(12)
        .visible(false)
        .build();
    holiday_group.append(&holiday_title);
    holiday_group.append(&holiday_list_box);

    let list_column = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    list_column.append(&list_box);
    list_column.append(&holiday_group);
    let list_clamp = adw::Clamp::builder().child(&list_column).build();
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&list_clamp)
        .vexpand(true)
        .build();

    // GNOME Clocks' empty state: a centered icon, headline and body text, and
    // a pill-shaped "Add Alarm…" button.
    let empty_add_button = gtk4::Button::builder()
        .label("Add Alarm…")
        .use_underline(true)
        .halign(gtk4::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    let empty_view = adw::StatusPage::builder()
        .icon_name("net.dunevent.Cajole-symbolic")
        .title("No Alarms")
        .child(&empty_add_button)
        .build();

    let stack = gtk4::Stack::builder().build();
    stack.add_named(&empty_view, Some("empty"));
    stack.add_named(&scrolled, Some("list"));

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    content.append(&stack);

    // Wraps the window's content so a toast (e.g. "Alarm set for 8 hours from
    // now") appears on the main window, as in GNOME Clocks, once the alarm
    // form has closed.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&content));

    let add_button = gtk4::Button::from_icon_name("list-add-symbolic");
    add_button.set_tooltip_text(Some("Add a new alarm"));

    // The primary menu holds "Manage Sounds…" and "About", which are
    // app-level, not per-alarm.
    let primary_menu = gio::Menu::new();
    primary_menu.append(Some("Manage Sounds…"), Some("app.manage-sounds"));
    primary_menu.append(Some("Holiday Mode…"), Some("app.holiday-mode"));
    primary_menu.append(Some("Settings…"), Some("app.settings"));
    primary_menu.append(Some("About Cajole"), Some("app.about"));
    let menu_button = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&primary_menu)
        .tooltip_text("Main Menu")
        .build();

    let header = adw::HeaderBar::builder().build();
    header.pack_start(&add_button);
    header.pack_end(&menu_button);

    // Revealed from `refresh_list` only while Holiday Mode is active today;
    // "Manage" reuses the menu's action.
    let holiday_banner = adw::Banner::builder()
        .title("Holiday Mode is active")
        .button_label("Manage")
        .action_name("app.holiday-mode")
        .revealed(false)
        .build();

    // The upcoming-holiday notice, independent of the banner above. It is
    // never shown once the holiday is active (`upcoming_holiday` returns
    // `None`), so the two are not revealed together. The title is overwritten
    // on each refresh; the builder's is a placeholder.
    let upcoming_holiday_banner = adw::Banner::builder()
        .title("Holiday Mode starts soon")
        .button_label("Manage")
        .action_name("app.holiday-mode")
        .revealed(false)
        .build();

    let silent_banner = adw::Banner::builder()
        .title("Vibration is off while the phone is in Silent mode")
        .button_label("Settings")
        .action_name("app.settings")
        .revealed(false)
        .build();

    let toolbar_view = adw::ToolbarView::builder().content(&toast_overlay).build();
    toolbar_view.add_top_bar(&header);
    toolbar_view.add_top_bar(&holiday_banner);
    toolbar_view.add_top_bar(&upcoming_holiday_banner);
    toolbar_view.add_top_bar(&silent_banner);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Cajole")
        .default_width(360)
        .default_height(640)
        .content(&toolbar_view)
        .build();

    // Closing the window hides it: Cajole must keep running in the background.
    // Returning `Stop` skips GTK's default close (which destroys the window);
    // the `hold()` in `main` keeps the app alive, and a later `activate` calls
    // `.present()` on this window.
    window.connect_close_request(|window| {
        window.set_visible(false);
        glib::Propagation::Stop
    });

    let manage_sounds_action = gio::SimpleAction::new("manage-sounds", None);
    manage_sounds_action.connect_activate({
        let window = window.clone();
        let path = path.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| {
            // No form is open, so `on_change` has nothing to do; a form opened
            // later loads the sound library at construction.
            show_sound_manager(&window, path.clone(), toast_overlay.clone(), || {});
        }
    });
    app.add_action(&manage_sounds_action);

    let settings_action = gio::SimpleAction::new("settings", None);
    settings_action.connect_activate({
        let window = window.clone();
        move |_, _| {
            show_settings(&window);
        }
    });
    app.add_action(&settings_action);

    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate({
        let window = window.clone();
        move |_, _| {
            show_about_dialog(&window);
        }
    });
    app.add_action(&about_action);

    // Probed once at startup, since both are properties of the session and
    // hardware. Defaults to `true` until the check completes: briefly showing
    // a control that does nothing is a smaller problem than hiding a working
    // one, and existing vibration or glow settings should not vanish while the
    // check runs.
    let vibration_available: Rc<Cell<bool>> = Rc::new(Cell::new(true));
    let brightness_available: Rc<Cell<bool>> = Rc::new(Cell::new(true));

    let refresh_list: RefreshFn = {
        let list_box = list_box.clone();
        let holiday_list_box = holiday_list_box.clone();
        let holiday_group = holiday_group.clone();
        let path = path.clone();
        let window = window.clone();
        let stack = stack.clone();
        let toast_overlay = toast_overlay.clone();
        let vibration_available = vibration_available.clone();
        let brightness_available = brightness_available.clone();
        let tracker = tracker.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        let holiday_banner = holiday_banner.clone();
        let upcoming_holiday_banner = upcoming_holiday_banner.clone();
        let silent_banner = silent_banner.clone();
        Rc::new(move || {
            refresh_list_impl(
                &list_box,
                &holiday_list_box,
                holiday_group.upcast_ref(),
                &stack,
                &path,
                &window,
                &toast_overlay,
                &vibration_available,
                &brightness_available,
                &tracker,
                &scheduler_handle_cell,
            );
            let settings = cajole::holiday::HolidaySettings::load(holiday_path()).unwrap_or_default();
            let now = chrono::Local::now();
            holiday_banner.set_revealed(settings.is_active_on(now.date_naive()));
            let upcoming = AlarmStore::load(&path)
                .ok()
                .and_then(|store| cajole::holiday::upcoming_holiday(store.alarms(), &settings, now));
            match upcoming {
                Some(info) => {
                    upcoming_holiday_banner
                        .set_title(&cajole::holiday::format_upcoming_holiday_banner(&info));
                    upcoming_holiday_banner.set_revealed(true);
                }
                None => upcoming_holiday_banner.set_revealed(false),
            }
            silent_banner.set_revealed(silent_banner_needed(&path));
        })
    };
    refresh_list();

    // Settings changes and feedbackd profile changes re-evaluate the
    // Silent banner without a full list refresh.
    let update_silent_banner_action = gio::SimpleAction::new("update-silent-banner", None);
    update_silent_banner_action.connect_activate({
        let silent_banner = silent_banner.clone();
        let path = path.clone();
        move |_, _| silent_banner.set_revealed(silent_banner_needed(&path))
    });
    app.add_action(&update_silent_banner_action);
    glib::MainContext::default().spawn_local({
        let silent_banner = silent_banner.clone();
        let path = path.clone();
        async move {
            let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
                return;
            };
            // Subscribed for the app's lifetime.
            let _subscription = conn.signal_subscribe(
                Some("org.sigxcpu.Feedback"),
                Some("org.freedesktop.DBus.Properties"),
                Some("PropertiesChanged"),
                Some("/org/sigxcpu/Feedback"),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, _| silent_banner.set_revealed(silent_banner_needed(&path)),
            );
        }
    });

    let holiday_mode_action = gio::SimpleAction::new("holiday-mode", None);
    holiday_mode_action.connect_activate({
        let window = window.clone();
        let path = path.clone();
        let refresh_list = refresh_list.clone();
        let toast_overlay = toast_overlay.clone();
        let vibration_available = vibration_available.clone();
        let brightness_available = brightness_available.clone();
        let tracker = tracker.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move |_, _| {
            show_holiday_wizard(
                &window,
                path.clone(),
                refresh_list.clone(),
                toast_overlay.clone(),
                vibration_available.clone(),
                brightness_available.clone(),
                tracker.clone(),
                scheduler_handle_cell.clone(),
            );
        }
    });
    app.add_action(&holiday_mode_action);

    // These two are the alarm's controls, registered on the application so
    // they work from a notification's action buttons, including on the lock
    // screen with no window open (see `SchedulerEvent::Ringing` below, where
    // the notification is sent). Both call [`handle_alarm_dismissed`] and
    // [`handle_alarm_snoozed`], which the in-app ring dialog also calls.
    let dismiss_alarm_action = gio::SimpleAction::new("dismiss-alarm", Some(glib::VariantTy::STRING));
    dismiss_alarm_action.connect_activate({
        let app = app.clone();
        let path = path.clone();
        let ringing = ringing.clone();
        let open_ring_dialogs = open_ring_dialogs.clone();
        let refresh_list = refresh_list.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move |_, parameter| {
            let Some(id_str) = parameter.and_then(|v| v.get::<String>()) else {
                return;
            };
            let Ok(alarm_id) = Uuid::parse_str(&id_str) else {
                return;
            };
            let app = app.clone();
            let path = path.clone();
            let ringing = ringing.clone();
            let open_ring_dialogs = open_ring_dialogs.clone();
            let refresh_list = refresh_list.clone();
            let scheduler_handle = scheduler_handle_cell.borrow().clone();
            glib::MainContext::default().spawn_local(async move {
                handle_alarm_dismissed(
                    &app,
                    &path,
                    &ringing,
                    &open_ring_dialogs,
                    &refresh_list,
                    scheduler_handle,
                    alarm_id,
                )
                .await;
            });
        }
    });
    app.add_action(&dismiss_alarm_action);

    let snooze_alarm_action = gio::SimpleAction::new("snooze-alarm", Some(glib::VariantTy::STRING));
    snooze_alarm_action.connect_activate({
        let app = app.clone();
        let path = path.clone();
        let ringing = ringing.clone();
        let tracker = tracker.clone();
        let open_ring_dialogs = open_ring_dialogs.clone();
        let refresh_list = refresh_list.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move |_, parameter| {
            let Some(id_str) = parameter.and_then(|v| v.get::<String>()) else {
                return;
            };
            let Ok(alarm_id) = Uuid::parse_str(&id_str) else {
                return;
            };
            let app = app.clone();
            let path = path.clone();
            let ringing = ringing.clone();
            let tracker = tracker.clone();
            let open_ring_dialogs = open_ring_dialogs.clone();
            let refresh_list = refresh_list.clone();
            let scheduler_handle = scheduler_handle_cell.borrow().clone();
            glib::MainContext::default().spawn_local(async move {
                handle_alarm_snoozed(
                    &app,
                    &path,
                    &ringing,
                    &tracker,
                    &open_ring_dialogs,
                    &refresh_list,
                    scheduler_handle,
                    alarm_id,
                )
                .await;
            });
        }
    });
    app.add_action(&snooze_alarm_action);

    // Dismissing an alarm before it rings differs from dismissing a ringing
    // one: there is no `RingSession` to stop and no ring dialog to close. A
    // one-time alarm is turned off, as after it rings; a repeating alarm skips
    // today's occurrence through `Alarm::skip_through` (a date, because a
    // stored target time reverted time edits for that occurrence; see that
    // field).
    let dismiss_early_action =
        gio::SimpleAction::new("dismiss-alarm-early", Some(glib::VariantTy::STRING));
    dismiss_early_action.connect_activate({
        let app = app.clone();
        let path = path.clone();
        let tracker = tracker.clone();
        let refresh_list = refresh_list.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move |_, parameter| {
            let Some(id_str) = parameter.and_then(|v| v.get::<String>()) else {
                return;
            };
            let Ok(alarm_id) = Uuid::parse_str(&id_str) else {
                return;
            };
            let app = app.clone();
            let path = path.clone();
            let tracker = tracker.clone();
            let refresh_list = refresh_list.clone();
            let scheduler_handle_cell = scheduler_handle_cell.clone();
            glib::MainContext::default().spawn_local(async move {
                app.withdraw_notification(&format!("upcoming-{alarm_id}"));
                if let Ok(mut store) = AlarmStore::load(&path) {
                    if let Some(alarm) = store.get(alarm_id) {
                        let mut updated = alarm.clone();
                        if updated.repeat.is_empty() {
                            updated.enabled = false;
                        } else {
                            updated.skip_through = Some(chrono::Local::now().date_naive());
                        }
                        if let Err(err) = store.upsert(updated) {
                            tracing::warn!(%err, "failed to dismiss alarm early");
                        }
                        // Without this, the poll loop's cached next trigger
                        // stays at the skipped time until something
                        // invalidates it, since `sync` computes a value only
                        // for alarms it is not tracking. Same as
                        // `scheduler::snooze`'s `forget` call.
                        tracker.borrow_mut().forget(alarm_id);
                    }
                }
                request_reschedule(&scheduler_handle_cell);
                refresh_list();
            });
        }
    });
    app.add_action(&dismiss_early_action);

    let open_add_form = {
        let path = path.clone();
        let refresh_list = refresh_list.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let vibration_available = vibration_available.clone();
        let brightness_available = brightness_available.clone();
        let tracker = tracker.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move || {
            // No blocking check: "(None)" is always available in the form's
            // sound picker, and that row flags itself (warning triangle,
            // `.warning` style) when selected, instead of gating the form on a
            // downloaded sound.
            show_alarm_form(
                &window,
                None,
                path.clone(),
                refresh_list.clone(),
                toast_overlay.clone(),
                vibration_available.clone(),
                brightness_available.clone(),
                tracker.clone(),
                scheduler_handle_cell.clone(),
                None,
                None,
            );
        }
    };
    add_button.connect_clicked({
        let open_add_form = open_add_form.clone();
        move |_| open_add_form()
    });
    empty_add_button.connect_clicked(move |_| open_add_form());

    // Starts the scheduler: one check immediately, then an infrequent fallback
    // poll. The primary wake mechanism is a `systemd --user` timer rescheduled
    // whenever the next needed time could change (see
    // `scheduler::SchedulerHandle`). No second thread or channel.
    //
    // Built and spawned right after `refresh_list` (its one prerequisite) and
    // before everything that is not a dependency of the scheduler: the
    // capability probe below, `window.present()` and the autostart prompt.
    // Verifying and rescheduling the wake timer at startup is alarm
    // correctness and should be first in line for the main loop.
    let scheduler_handle = {
        let window = window.clone();
        let app = app.clone();
        let path = path.clone();
        let open_ring_dialogs = open_ring_dialogs.clone();
        let refresh_list = refresh_list.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        scheduler::SchedulerHandle::new(path.clone(), ringing.clone(), tracker.clone(), move |event| {
            let on_ringing_window = &window;
            let on_ringing_app = &app;
            let on_ringing_path = &path;
            let on_ringing_ringing = &ringing;
            let on_ringing_tracker = &tracker;
            let on_ringing_dialogs = &open_ring_dialogs;
            let on_ringing_refresh = &refresh_list;
            let on_ringing_scheduler_handle_cell = &scheduler_handle_cell;
            match event {
                scheduler::SchedulerEvent::Ringing { alarm_id, label } => {
                    // Clears a still-showing "upcoming" prompt for this alarm
                    // immediately instead of waiting for the next poll, which
                    // would also clear it. Withdrawing an already-gone
                    // notification is a no-op.
                    on_ringing_app.withdraw_notification(&format!("upcoming-{alarm_id}"));

                    // This is the alarm's always-working UI: notification
                    // actions run as app-level GActions with no window, so
                    // they work from the lock screen, as GNOME Clocks rings on
                    // Phosh. The `.desktop` file under `data/` must be
                    // installed (see `data/README.md`), or GNOME Shell silently
                    // drops the notification.
                    let title = if label.trim().is_empty() { "Alarm" } else { &label };
                    let notification = gio::Notification::new(title);
                    notification.set_body(Some("Ringing — snooze or dismiss below."));
                    notification.set_priority(gio::NotificationPriority::Urgent);
                    // Tapping the notification body opens the app. Forcing
                    // focus without user interaction is blocked by Wayland's
                    // focus-stealing prevention.
                    notification.set_default_action("app.open");
                    let id_variant = alarm_id.to_string().to_variant();
                    notification.add_button_with_target_value(
                        "Snooze",
                        "app.snooze-alarm",
                        Some(&id_variant),
                    );
                    notification.add_button_with_target_value(
                        "Dismiss",
                        "app.dismiss-alarm",
                        Some(&id_variant),
                    );
                    on_ringing_app.send_notification(Some(&alarm_id.to_string()), &notification);

                    // A convenience for when Cajole is already in the
                    // foreground; a locked phone relies on the notification.
                    show_ring_dialog(
                        &on_ringing_window,
                        &on_ringing_app,
                        alarm_id,
                        &label,
                        on_ringing_path.clone(),
                        on_ringing_ringing.clone(),
                        on_ringing_tracker.clone(),
                        on_ringing_dialogs.clone(),
                        on_ringing_refresh.clone(),
                        on_ringing_scheduler_handle_cell.clone(),
                    );
                }
                scheduler::SchedulerEvent::AutoStopped { alarm_id } => {
                    on_ringing_app.withdraw_notification(&alarm_id.to_string());
                    if let Some(dialog) = on_ringing_dialogs.borrow_mut().remove(&alarm_id) {
                        dialog.close();
                    }
                    on_ringing_refresh();
                }
                scheduler::SchedulerEvent::UpcomingAlarmCleared { alarm_id } => {
                    on_ringing_app.withdraw_notification(&format!("upcoming-{alarm_id}"));
                }
                scheduler::SchedulerEvent::UpcomingAlarm { alarm_id, label, next_trigger } => {
                    let title = if label.trim().is_empty() { "Upcoming Alarm".to_string() } else { label };
                    // An absolute time, not "X minutes from now", which goes
                    // stale while the notification sits unread. AOSP's
                    // DeskClock uses the formatted time for the same reason.
                    let use_12h = system_uses_12h_clock();
                    let when = next_trigger
                        .format(if use_12h { "%l:%M %p" } else { "%H:%M" })
                        .to_string();
                    let when = when.trim_start();
                    let notification = gio::Notification::new(&title);
                    notification.set_body(Some(&format!("Scheduled for {when}.")));
                    notification.set_priority(gio::NotificationPriority::Normal);
                    let id_variant = alarm_id.to_string().to_variant();
                    notification.add_button_with_target_value(
                        "Dismiss Early",
                        "app.dismiss-alarm-early",
                        Some(&id_variant),
                    );
                    // A different notification id from the "ringing" one this
                    // alarm will get later: this one can be ignored and left
                    // to expire, the ringing one cannot, and a shared id could
                    // let one replace the other.
                    on_ringing_app.send_notification(
                        Some(&format!("upcoming-{alarm_id}")),
                        &notification,
                    );
                }
            }
        })
    };
    *scheduler_handle_cell.borrow_mut() = Some(scheduler_handle.clone());

    // Registered on the app, not tied to a window: the systemd wake timer's
    // command pokes it (`gdbus call ... org.gtk.Actions.Activate
    // check-schedule`), and anything else can force a recheck. Takes no
    // parameters; the check reads the alarm store fresh.
    let check_schedule_action = gio::SimpleAction::new("check-schedule", None);
    check_schedule_action.connect_activate({
        let scheduler_handle = scheduler_handle.clone();
        move |_, _| {
            let scheduler_handle = scheduler_handle.clone();
            glib::MainContext::default().spawn_local(async move {
                scheduler::check_and_reschedule(&scheduler_handle).await;
            });
        }
    });
    app.add_action(&check_schedule_action);

    // A GAction so a notification's default action (tapping its body) has
    // something to activate: `app.activate()` runs the handler a second launch
    // would trigger, which calls `.present()` on the active window. Tapping a
    // notification is a compositor-recognized user action; `.present()` from
    // an internal handler is not, and Wayland's focus-stealing prevention
    // stops a background process from granting itself focus.
    let open_action = gio::SimpleAction::new("open", None);
    open_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            app.activate();
        }
    });
    app.add_action(&open_action);

    glib::MainContext::default().spawn_local({
        let scheduler_handle = scheduler_handle.clone();
        async move {
            scheduler::run(scheduler_handle).await;
        }
    });
    // Spawned after the scheduler, not before: GLib polls newly spawned tasks
    // roughly in queue order once the main loop runs, so this ordering gets
    // the scheduler's first check as early as possible. This probe matters for
    // the alarm form, which cannot be open this early, so it can run second.
    glib::MainContext::default().spawn_local({
        let vibration_available = vibration_available.clone();
        let brightness_available = brightness_available.clone();
        async move {
            if let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await {
                vibration_available.set(cajole::feedback::vibration_available(&conn).await);
                brightness_available.set(cajole::display::brightness_available(&conn).await);
            }
        }
    });

    if hidden {
        // No autostart prompt (it only matters when someone is looking at the
        // app). The window is built and returned either way; presenting it is
        // the caller's decision, so every activation goes through one
        // presenting path.
        return window;
    }

    maybe_prompt_for_autostart(&window);
    window
}

/// Builds one alarm row as the main window shows it: a three-line layout
/// (label, time, repeat summary), status badges and an inline enable switch
/// that writes to disk. `on_change` refreshes whatever view the caller owns
/// after the switch or an edit. It is generic instead of calling
/// `refresh_list_impl` so the row also works in the Holiday Mode wizard's page
/// 3, which refreshes its own list and the main window behind it.
///
/// `window` is generic over any GTK window so a row in the wizard opens its
/// edit form `transient_for` the wizard instead of the main window. Two
/// sibling dialogs transient for the same ancestor were the leading suspect
/// for an intermittent freeze when closing the wizard.
#[allow(clippy::too_many_arguments)]
fn build_alarm_row<W: IsA<gtk4::Window> + Clone + 'static>(
    alarm: &Alarm,
    use_12h: bool,
    holiday_active: bool,
    path: &std::path::Path,
    window: &W,
    toast_overlay: &adw::ToastOverlay,
    vibration_available: &Rc<Cell<bool>>,
    brightness_available: &Rc<Cell<bool>>,
    tracker: &SharedTracker,
    scheduler_handle_cell: &Rc<RefCell<Option<scheduler::SchedulerHandle>>>,
    on_change: RefreshFn,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().activatable(true).build();
    // Suspended by Holiday Mode: dimmed and labeled, separate from the enable
    // switch. Always `false` for a `holiday_only` alarm, since nothing sets
    // `holiday_excluded` on one.
    let suspended = alarm.holiday_excluded && holiday_active;
    if suspended {
        row.add_css_class("dim-label");
    }

    // Custom three-line layout in place of ActionRow's title/subtitle, as in
    // GNOME Clocks' alarm row: an optional bold name, the time as the large
    // dominant element, then the repeat summary in small dim text.
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .valign(gtk4::Align::Center)
        .spacing(2)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    if !alarm.label.trim().is_empty() {
        let title_label = gtk4::Label::builder()
            .label(&alarm.label)
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        content.append(&title_label);
    }
    let time_label = gtk4::Label::builder()
        .label(
            alarm
                .time
                .format(if use_12h { "%l:%M %p" } else { "%H:%M" })
                .to_string(),
        )
        .halign(gtk4::Align::Start)
        .css_classes(["cajole-alarm-time"])
        .build();
    content.append(&time_label);
    let repeat_label = gtk4::Label::builder()
        .label(if suspended {
            format!("{}, off during Holiday Mode", repeat_summary(&alarm.repeat))
        } else {
            repeat_summary(&alarm.repeat)
        })
        .halign(gtk4::Align::Start)
        .css_classes(["dim-label", "caption"])
        .build();
    content.append(&repeat_label);
    row.add_prefix(&content);

    // Status badges showing what the alarm will do without opening it: whether
    // it has a tone (not "(None)") and which parts of Gentle Wake are in play.
    // Vibration and glow reflect their own toggles; Gentle Wake mode only
    // decides whether an enabled one ramps or holds constant.
    let has_sound = alarm.play_sound && alarm.sound_file.is_some();
    let has_vibration = alarm.vibration_enabled;
    let has_glow = alarm.glow_enabled;
    // The Gentle Mode badge needs an effect behind it: vibration and glow
    // stand on their own, but volume ramping needs a sound. "Enabled with
    // nothing else on" and "only sound on, with no sound chosen" leave Gentle
    // Mode nothing to do.
    let gentle_mode_meaningful =
        alarm.gentle_wake_enabled && (has_vibration || has_glow || has_sound);
    let badges = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .valign(gtk4::Align::Center)
        .build();
    let add_badge = |icon_name: &str, tooltip: &str| {
        let icon = gtk4::Image::from_icon_name(icon_name);
        icon.set_tooltip_text(Some(tooltip));
        icon.add_css_class("dim-label");
        badges.append(&icon);
    };
    if gentle_mode_meaningful {
        add_badge("sentiment-satisfied-symbolic", "Gentle Mode");
    }
    if has_sound {
        add_badge("music-note-outline-symbolic", "Alarm tone set");
    }
    if has_vibration {
        add_badge("phone-vibrate-symbolic", "Vibration");
    }
    if has_glow {
        add_badge("image-adjust-brightness-symbolic", "Screen glow");
    }
    // No sound, vibration or glow: a plain warning icon, without the error
    // styling of the edit form's "(None)" row, flagging that the alarm has
    // nothing to wake anyone with.
    if !has_sound && !has_vibration && !has_glow {
        add_badge(
            "dialog-warning-symbolic",
            "No sound, vibration, or glow set — this alarm won't wake you",
        );
    }
    row.add_suffix(&badges);

    // Inline enable switch, as in GNOME Clocks' alarm row, so a person can
    // toggle it without opening the edit screen.
    let enabled_switch = gtk4::Switch::builder()
        .active(alarm.enabled)
        .valign(gtk4::Align::Center)
        .tooltip_text("Enable Alarm")
        .build();
    row.add_suffix(&enabled_switch);

    enabled_switch.connect_state_set({
        let path = path.to_path_buf();
        let toast_overlay = toast_overlay.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        let alarm_id = alarm.id;
        let on_change = on_change.clone();
        move |_, new_state| {
            if let Ok(mut store) = AlarmStore::load(&path) {
                if let Some(current) = store.get(alarm_id) {
                    let mut updated = current.clone();
                    updated.enabled = new_state;
                    let stays_off = new_state
                        && updated.holiday_excluded
                        && cajole::holiday::HolidaySettings::load(holiday_path())
                            .unwrap_or_default()
                            .is_active_on(chrono::Local::now().date_naive());
                    if let Err(err) = store.upsert(updated) {
                        tracing::error!(%err, "failed to toggle alarm enabled state");
                    } else if stays_off {
                        toast_overlay.add_toast(adw::Toast::new(
                            "Enabled, but it stays off until Holiday Mode ends",
                        ));
                    }
                }
            }
            request_reschedule(&scheduler_handle_cell);
            on_change();
            glib::Propagation::Proceed
        }
    });

    row.connect_activated({
        let window = window.clone();
        let path = path.to_path_buf();
        let toast_overlay = toast_overlay.clone();
        let vibration_available = vibration_available.clone();
        let brightness_available = brightness_available.clone();
        let tracker = tracker.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        let alarm = alarm.clone();
        let on_change = on_change.clone();
        move |_| {
            let holiday_bounds = alarm
                .holiday_only
                .then(|| {
                    let settings =
                        cajole::holiday::HolidaySettings::load(holiday_path()).unwrap_or_default();
                    settings.start.zip(settings.end)
                })
                .flatten();
            show_alarm_form(
                &window,
                Some(alarm.clone()),
                path.clone(),
                on_change.clone(),
                toast_overlay.clone(),
                vibration_available.clone(),
                brightness_available.clone(),
                tracker.clone(),
                scheduler_handle_cell.clone(),
                holiday_bounds,
                None,
            );
        }
    });

    row
}

/// Rebuilds the alarm list from disk and keeps the empty/list `GtkStack` in
/// sync with whether any alarms exist. A plain function, not only a
/// `refresh_list` closure, so each row's click handler can trigger a rebuild
/// after an edit without capturing the closure recursively, and every refresh
/// path (toggle, edit, delete) updates the stack the same way.
#[allow(clippy::too_many_arguments)]
fn refresh_list_impl(
    list_box: &gtk4::ListBox,
    holiday_list_box: &gtk4::ListBox,
    holiday_group: &gtk4::Widget,
    stack: &gtk4::Stack,
    path: &std::path::Path,
    window: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
    vibration_available: &Rc<Cell<bool>>,
    brightness_available: &Rc<Cell<bool>>,
    tracker: &SharedTracker,
    scheduler_handle_cell: &Rc<RefCell<Option<scheduler::SchedulerHandle>>>,
) {
    tracing::debug!("refresh_list_impl: start");
    while let Some(row) = list_box.row_at_index(0) {
        list_box.remove(&row);
    }
    while let Some(row) = holiday_list_box.row_at_index(0) {
        holiday_list_box.remove(&row);
    }
    let use_12h = system_uses_12h_clock();
    let holiday_settings = cajole::holiday::HolidaySettings::load(holiday_path()).unwrap_or_default();
    let now = chrono::Local::now();
    if let Ok(mut store) = AlarmStore::load(path) {
        if let Err(err) = cajole::holiday::sweep(&mut store, &holiday_settings, now) {
            tracing::warn!(%err, "holiday housekeeping failed");
        }
    }
    tracing::debug!("refresh_list_impl: holiday sweep done, building rows");
    let holiday_active = holiday_settings.is_active_on(now.date_naive());
    let has_alarms = match AlarmStore::load(path) {
        Ok(store) => {
            // Holiday-only alarms get their own block below the regular ones,
            // only while Holiday Mode is active; before then the list looks as
            // usual.
            let regular: Vec<&Alarm> = store.alarms().iter().filter(|a| !a.holiday_only).collect();
            let holiday_alarms: Vec<&Alarm> = if holiday_active {
                store.alarms().iter().filter(|a| a.holiday_only).collect()
            } else {
                Vec::new()
            };
            holiday_group.set_visible(!holiday_alarms.is_empty());
            for alarm in regular.iter().chain(holiday_alarms.iter()).copied() {
                let target_list_box = if alarm.holiday_only {
                    holiday_list_box
                } else {
                    list_box
                };
                let on_change: RefreshFn = {
                    let list_box = list_box.clone();
                    let holiday_list_box = holiday_list_box.clone();
                    let holiday_group = holiday_group.clone();
                    let stack = stack.clone();
                    let path = path.to_path_buf();
                    let window = window.clone();
                    let toast_overlay = toast_overlay.clone();
                    let vibration_available = vibration_available.clone();
                    let brightness_available = brightness_available.clone();
                    let tracker = tracker.clone();
                    let scheduler_handle_cell = scheduler_handle_cell.clone();
                    Rc::new(move || {
                        refresh_list_impl(
                            &list_box,
                            &holiday_list_box,
                            holiday_group.upcast_ref(),
                            &stack,
                            &path,
                            &window,
                            &toast_overlay,
                            &vibration_available,
                            &brightness_available,
                            &tracker,
                            &scheduler_handle_cell,
                        );
                    })
                };
                let row = build_alarm_row(
                    alarm,
                    use_12h,
                    holiday_active,
                    path,
                    window,
                    toast_overlay,
                    vibration_available,
                    brightness_available,
                    tracker,
                    scheduler_handle_cell,
                    on_change,
                );
                target_list_box.append(&row);
            }
            !regular.is_empty() || !holiday_alarms.is_empty()
        }
        Err(err) => {
            tracing::error!(%err, "failed to load alarm store for display");
            false
        }
    };
    stack.set_visible_child_name(if has_alarms { "list" } else { "empty" });
    tracing::debug!("refresh_list_impl: done");
}

/// Whether the desktop uses a 12-hour clock, from
/// `org.gnome.desktop.interface` `clock-format`, which the shell and other
/// GNOME apps honor. In a Flatpak the value comes from the Settings portal,
/// since GSettings returns the runtime default. The schema comes from
/// `gsettings-desktop-schemas`, present on GNOME and Phosh but not guaranteed
/// elsewhere; a missing schema falls back to 24h, because `gio::Settings::new`
/// aborts the process for a missing schema, so `SettingsSchemaSource` is
/// checked first.
fn system_uses_12h_clock() -> bool {
    if cajole::sandbox::is_flatpak() {
        if let Some(format) =
            cajole::sandbox::read_portal_setting("org.gnome.desktop.interface", "clock-format")
        {
            return format == "12h";
        }
    }
    let Some(source) = gio::SettingsSchemaSource::default() else {
        return false;
    };
    if source
        .lookup("org.gnome.desktop.interface", true)
        .is_none()
    {
        return false;
    }
    let settings = gio::Settings::new("org.gnome.desktop.interface");
    settings.string("clock-format") == "12h"
}

/// Converts a 24-hour `hour` (0-23) to what a spin row displays: unchanged in
/// 24h mode, or 1-12 in 12h mode (0 and 12 both display as 12).
fn display_hour(hour: u32, use_12h: bool) -> u32 {
    if !use_12h {
        return hour;
    }
    let h12 = hour % 12;
    if h12 == 0 {
        12
    } else {
        h12
    }
}

/// The inverse of [`display_hour`]: given the spin row's value (1-12 in 12h
/// mode, 0-23 in 24h mode) and, in 12h mode, whether PM is selected, computes
/// the 24-hour hour stored on the `Alarm`.
fn hour_24_from_form(displayed_hour: u32, is_pm: bool, use_12h: bool) -> u32 {
    if !use_12h {
        return displayed_hour;
    }
    match (displayed_hour, is_pm) {
        (12, false) => 0,  // 12 AM -> 00:xx
        (12, true) => 12,  // 12 PM -> 12:xx
        (h, false) => h,   // 1-11 AM -> 1-11
        (h, true) => h + 12, // 1-11 PM -> 13-23
    }
}

/// glibc's `_NL_TIME_FIRST_WEEKDAY` langinfo item. The `libc` crate does not
/// expose it (a glibc-internal locale extension), so the value comes from
/// glibc's macro `_NL_ITEM(LC_TIME, 104)` = 131176, checked by compiling a C
/// program against the system headers. A runtime check returned Sunday, Monday
/// and Saturday for `en_US.UTF-8`, `de_DE.UTF-8` and `ar_EG.UTF-8`.
const NL_TIME_FIRST_WEEKDAY: libc::c_int = 131176;

/// The effective first day of the week: the GNOME override if set, else the
/// locale default. Callers use this; [`locale_first_weekday`] and
/// [`gsettings_week_start_override`] are its building blocks.
fn first_weekday() -> Weekday {
    gsettings_week_start_override().unwrap_or_else(locale_first_weekday)
}

/// A user-set first day of the week from GNOME's `org.gnome.desktop.calendar`
/// `week-start-day` key (the Settings app's control), which takes precedence
/// over the locale. In a Flatpak it is read through the Settings portal. The
/// key is new, so the schema and key are checked before use. Any value that is
/// not a day name (including the unset default) means no override and falls
/// through to the locale value.
fn gsettings_week_start_override() -> Option<Weekday> {
    let day = if cajole::sandbox::is_flatpak() {
        cajole::sandbox::read_portal_setting("org.gnome.desktop.calendar", "week-start-day")
    } else {
        None
    };
    if let Some(day) = day {
        return weekday_from_name(&day);
    }
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup("org.gnome.desktop.calendar", true)?;
    if !schema.has_key("week-start-day") {
        return None;
    }
    let settings = gio::Settings::new("org.gnome.desktop.calendar");
    weekday_from_name(settings.string("week-start-day").as_str())
}

/// Any value that is not a day name, such as the unset `default`, is no override.
fn weekday_from_name(name: &str) -> Option<Weekday> {
    match name.to_lowercase().as_str() {
        "monday" => Some(Weekday::Mon),
        "tuesday" => Some(Weekday::Tue),
        "wednesday" => Some(Weekday::Wed),
        "thursday" => Some(Weekday::Thu),
        "friday" => Some(Weekday::Fri),
        "saturday" => Some(Weekday::Sat),
        "sunday" => Some(Weekday::Sun),
        _ => None,
    }
}

/// The locale's first day of the week from glibc's `_NL_TIME_FIRST_WEEKDAY`
/// (as `GtkCalendar` does): a 1-7 index into the Sunday-based `DAY_1..DAY_7`
/// sequence (`1` = Sunday, `2` = Monday, ...). Out-of-range values fall back
/// to Monday (ISO 8601) instead of panicking.
fn locale_first_weekday() -> Weekday {
    // SAFETY: `nl_langinfo` returns a pointer into static glibc locale data;
    // one byte is read and the pointer is not kept.
    let value = unsafe {
        let ptr = libc::nl_langinfo(NL_TIME_FIRST_WEEKDAY);
        if ptr.is_null() {
            return Weekday::Mon;
        }
        *(ptr as *const u8)
    };
    match value {
        1 => Weekday::Sun,
        2 => Weekday::Mon,
        3 => Weekday::Tue,
        4 => Weekday::Wed,
        5 => Weekday::Thu,
        6 => Weekday::Fri,
        7 => Weekday::Sat,
        _ => Weekday::Mon,
    }
}

/// The seven weekdays in order starting from `first`. Pure (unlike
/// [`locale_first_weekday`]), so the rotation can be unit-tested without a
/// specific locale installed.
fn week_order_from(first: Weekday) -> [Weekday; 7] {
    let mut days = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];
    let offset = days.iter().position(|d| *d == first).unwrap_or(0);
    days.rotate_left(offset);
    days
}

fn day_abbreviation(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

fn repeat_summary(repeat: &RepeatDays) -> String {
    if repeat.is_empty() {
        return "Once".to_string();
    }
    if *repeat == RepeatDays::EVERY_DAY {
        return "Every day".to_string();
    }
    if *repeat == RepeatDays::WEEKDAYS {
        return "Weekdays".to_string();
    }
    if *repeat == RepeatDays::WEEKENDS {
        return "Weekends".to_string();
    }
    week_order_from(first_weekday())
        .into_iter()
        .filter(|day| repeat.contains(*day))
        .map(day_abbreviation)
        .collect::<Vec<_>>()
        .join(", ")
}

struct DayToggles {
    container: gtk4::Box,
    buttons: Vec<(Weekday, gtk4::ToggleButton)>,
}

fn build_day_toggles(selected: RepeatDays) -> DayToggles {
    let days = [
        (Weekday::Mon, "M"),
        (Weekday::Tue, "T"),
        (Weekday::Wed, "W"),
        (Weekday::Thu, "T"),
        (Weekday::Fri, "F"),
        (Weekday::Sat, "S"),
        (Weekday::Sun, "S"),
    ];

    let container = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk4::Align::Center)
        .margin_top(6)
        .margin_bottom(6)
        .build();

    let buttons: Vec<(Weekday, gtk4::ToggleButton)> = days
        .into_iter()
        .map(|(day, label)| {
            let button = gtk4::ToggleButton::builder()
                .label(label)
                .active(selected.contains(day))
                .css_classes(["circular"])
                .tooltip_text(format!("{day}"))
                .build();
            container.append(&button);
            (day, button)
        })
        .collect();

    DayToggles { container, buttons }
}

fn collect_repeat_days(toggles: &DayToggles) -> RepeatDays {
    let mut repeat = RepeatDays::NONE;
    for (day, button) in &toggles.buttons {
        if button.is_active() {
            repeat.insert(*day);
        }
    }
    repeat
}

/// A port of GNOME Clocks' `AmPmToggleButton`: one button that swaps between
/// "AM" and "PM" on click, with no popup to open. Matching that interaction is
/// the point.
struct AmPmToggle {
    button: gtk4::Button,
    is_pm: Rc<std::cell::Cell<bool>>,
}

impl AmPmToggle {
    fn new(initially_pm: bool) -> Self {
        let am_label = gtk4::Label::new(Some("AM"));
        let pm_label = gtk4::Label::new(Some("PM"));
        let stack = gtk4::Stack::new();
        stack.add_child(&am_label);
        stack.add_child(&pm_label);

        let button = gtk4::Button::builder()
            .child(&stack)
            .css_classes(["cajole-ampm-toggle"])
            .valign(gtk4::Align::Center)
            .halign(gtk4::Align::Center)
            .build();

        let is_pm = Rc::new(std::cell::Cell::new(initially_pm));
        stack.set_visible_child(if initially_pm { &pm_label } else { &am_label });

        button.connect_clicked({
            let is_pm = is_pm.clone();
            let stack = stack.clone();
            let am_label = am_label.clone();
            let pm_label = pm_label.clone();
            move |_| {
                let new_value = !is_pm.get();
                is_pm.set(new_value);
                stack.set_visible_child(if new_value { &pm_label } else { &am_label });
            }
        });

        AmPmToggle { button, is_pm }
    }

    fn is_pm(&self) -> bool {
        self.is_pm.get()
    }
}

/// Formats a `GtkSpinButton`'s text with a leading zero (`"7"` -> `"07"`), as
/// GNOME Clocks' `show_leading_zeros` does, via the spin button's `output`
/// signal.
fn connect_leading_zeros(spin: &gtk4::SpinButton) {
    spin.connect_output(|spin| {
        spin.set_text(&format!("{:02}", spin.value() as i32));
        glib::Propagation::Stop
    });
}

/// Numeric on-screen keyboard instead of the alphanumeric one, for a duration
/// or percentage `AdwSpinRow` (the alarm's time picker, a bare
/// `GtkSpinButton`, gets it already). `input-purpose` is a property of
/// `GtkText`, and neither `AdwSpinRow` nor the `GtkSpinButton` it wraps
/// exposes it: `set_property` on either panics with "property not found".
/// Reaching the `GtkText` takes two `GtkEditable::delegate()` levels: the row
/// delegates to its `GtkSpinButton`, which delegates to its `GtkText`.
fn set_spin_row_numeric_input(row: &adw::SpinRow) {
    let row_editable: &gtk4::Editable = row.upcast_ref();
    let Some(spin_delegate) = row_editable.delegate() else {
        return;
    };
    let Ok(spin_editable) = spin_delegate.dynamic_cast::<gtk4::Editable>() else {
        return;
    };
    let Some(text_delegate) = spin_editable.delegate() else {
        return;
    };
    text_delegate.set_property("input-purpose", gtk4::InputPurpose::Digits);
}

/// Builds the Clocks-style time picker: two vertical `GtkSpinButton`s (the
/// +/number/- layout) joined by a colon, with an [`AmPmToggle`] appended when
/// `use_12h`. Returns the container and handles to read the values on save.
struct TimePicker {
    container: gtk4::Box,
    hour: gtk4::SpinButton,
    minute: gtk4::SpinButton,
    am_pm: Option<AmPmToggle>,
}

fn build_time_picker(base_time: NaiveTime, use_12h: bool) -> TimePicker {
    let hour_adjustment = gtk4::Adjustment::new(
        display_hour(base_time.hour(), use_12h) as f64,
        if use_12h { 1.0 } else { 0.0 },
        if use_12h { 12.0 } else { 23.0 },
        1.0,
        10.0,
        0.0,
    );
    let hour = gtk4::SpinButton::builder()
        .adjustment(&hour_adjustment)
        .orientation(gtk4::Orientation::Vertical)
        .numeric(true)
        .wrap(true)
        .xalign(0.5)
        .width_request(60)
        .tooltip_text("Hour")
        .css_classes(["cajole-alarm-setup-time"])
        .build();
    connect_leading_zeros(&hour);

    let minute_adjustment =
        gtk4::Adjustment::new(base_time.minute() as f64, 0.0, 59.0, 1.0, 10.0, 0.0);
    let minute = gtk4::SpinButton::builder()
        .adjustment(&minute_adjustment)
        .orientation(gtk4::Orientation::Vertical)
        .numeric(true)
        .wrap(true)
        .xalign(0.5)
        .width_request(60)
        .tooltip_text("Minute")
        .css_classes(["cajole-alarm-setup-time"])
        .build();
    connect_leading_zeros(&minute);

    let colon = gtk4::Label::builder()
        .label("\u{2236}") // U+2236 RATIO, the exact glyph Clocks itself uses
        .css_classes(["cajole-alarm-setup-time"])
        .build();

    let container = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk4::Align::Center)
        .margin_top(6)
        .margin_bottom(12)
        .build();
    // Forced left-to-right regardless of UI direction: this is an [hour] :
    // [minute] display, and RTL would read minute-then-hour. Clocks does the
    // same.
    container.set_direction(gtk4::TextDirection::Ltr);
    container.append(&hour);
    container.append(&colon);
    container.append(&minute);

    let am_pm = if use_12h {
        let toggle = AmPmToggle::new(base_time.hour() >= 12);
        container.append(&toggle.button);
        Some(toggle)
    } else {
        None
    };

    TimePicker {
        container,
        hour,
        minute,
        am_pm,
    }
}

/// Stylesheet mirroring GNOME Clocks' `.clocks-alarm-setup-time`,
/// `.clocks-ampm-toggle-button` and `.alarm-time` classes, so the ported
/// widgets are as prominent as the originals.
const APP_CSS: &str = "
.cajole-alarm-setup-time { font-size: 32pt; }
.cajole-ampm-toggle { font-size: 18pt; }
.cajole-alarm-time { font-size: 2.5em; font-weight: 300; }
";

fn install_time_picker_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(APP_CSS);
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // Default for a bare or headless session with no icon theme set, where
        // symbolic icon lookups (including the empty-state icon) would fail. A
        // normal GNOME or Phosh session has a theme.
        if gtk4::IconTheme::for_display(&display).theme_name().is_empty() {
            gtk4::IconTheme::for_display(&display).set_theme_name(Some("Adwaita"));
        }
    }
}

/// Shows the add/edit form. `existing` is `None` for a new alarm (seeded with
/// defaults) or `Some(alarm)` to edit it in place, keeping its id.
///
/// `holiday_bounds`, when `Some((start, end))`, means the form was opened from
/// the Holiday Mode wizard's "+ Holiday Alarm" flow: the saved alarm gets
/// `holiday_only` set regardless of `existing`, a banner says it will be
/// deleted once exhausted, and day toggles for weekdays absent from `[start,
/// end]` are disabled, since selecting one would create an alarm with nothing
/// to ring for.
#[allow(clippy::too_many_arguments)]
fn show_alarm_form<W: IsA<gtk4::Window> + Clone + 'static>(
    parent: &W,
    existing: Option<Alarm>,
    store_path: PathBuf,
    refresh_list: RefreshFn,
    toast_overlay: adw::ToastOverlay,
    vibration_available: Rc<Cell<bool>>,
    brightness_available: Rc<Cell<bool>>,
    tracker: SharedTracker,
    scheduler_handle_cell: Rc<RefCell<Option<scheduler::SchedulerHandle>>>,
    holiday_bounds: Option<(NaiveDate, NaiveDate)>,
    on_saved: Option<Rc<dyn Fn(Uuid)>>,
) {
    let holiday_only_override = holiday_bounds.map(|_| true);
    let is_new = existing.is_none();
    let base = existing
        .unwrap_or_else(|| Alarm::new("", NaiveTime::from_hms_opt(7, 0, 0).unwrap()));
    let alarm_id = base.id;

    let label_row = adw::EntryRow::builder()
        .title("Label")
        .text(base.label.clone())
        .build();

    let use_12h = system_uses_12h_clock();
    let time_picker = build_time_picker(base.time, use_12h);

    let day_toggles = build_day_toggles(base.repeat);

    // A day absent from the holiday window would create a `holiday_only` alarm
    // with nothing to ring for, so it is disabled up front. At most 7 days are
    // checked, which covers every weekday.
    if let Some((start, end)) = holiday_bounds {
        let mut present = RepeatDays::NONE;
        let mut day = start;
        for _ in 0..7 {
            if day > end {
                break;
            }
            present.insert(day.weekday());
            day = day.succ_opt().unwrap_or(day);
        }
        for (weekday, button) in &day_toggles.buttons {
            if !present.contains(*weekday) {
                button.set_active(false);
                button.set_sensitive(false);
            }
        }
    }

    let holiday_banner = holiday_bounds.map(|_| {
        adw::Banner::builder()
            .title("Holiday Alarms are deleted once Holiday Mode ends")
            .revealed(true)
            .build()
    });

    // The time picker and day toggles are plain `gtk4::Box` widgets, not
    // `AdwActionRow`-style rows. `AdwPreferencesGroup` does not render them
    // when mixed with rows in one group, though each renders as a group's only
    // child. Each stays in its own headerless group; the continuous-list look
    // comes from zero spacing on the body.
    let time_group = adw::PreferencesGroup::builder().build();
    time_group.add(&time_picker.container);
    let repeat_group = adw::PreferencesGroup::builder().build();
    repeat_group.add(&day_toggles.container);

    let general_group = adw::PreferencesGroup::builder().build();
    general_group.add(&label_row);

    let alarm_duration_row = adw::SpinRow::builder()
        .title("Alarm duration (minutes)")
        .adjustment(&gtk4::Adjustment::new(
            base.alarm_duration_minutes as f64,
            1.0,
            60.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&alarm_duration_row);
    general_group.add(&alarm_duration_row);

    // Gentle Wake mode. Orthogonal to which of sound, vibration and glow are
    // enabled: it only controls whether an enabled one ramps (start percent to
    // end percent over the duration) or holds one constant level while the
    // alarm rings. The duration row stays because ramping needs it, and is
    // hidden with everything else when the mode is off.
    let gentle_wake_enabled_row = adw::SwitchRow::builder()
        .title("Gentle wake mode")
        .active(base.gentle_wake_enabled)
        .build();
    general_group.add(&gentle_wake_enabled_row);
    let gentle_wake_duration_row = adw::SpinRow::builder()
        .title("Gentle wake duration (seconds)")
        .adjustment(&gtk4::Adjustment::new(
            base.gentle_wake_duration_secs as f64,
            1.0,
            600.0,
            1.0,
            10.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&gentle_wake_duration_row);
    general_group.add(&gentle_wake_duration_row);

    // Play sound
    let play_sound_row = adw::SwitchRow::builder()
        .title("Play sound")
        .active(base.play_sound)
        .build();

    // The picker offers only sounds that have a local file ready to play. The
    // gear button next to it adds, downloads or removes entries.
    let sound_paths: Rc<RefCell<Vec<Option<String>>>> = Rc::new(RefCell::new(Vec::new()));
    let sound_row = adw::ComboRow::builder().title("\u{a0}\u{a0}\u{a0}\u{a0}Sound").build();

    // A Play/Stop toggle, matching the Sound Manager's rows. Built before
    // `update_sound_warning` so that function can also control this button's
    // sensitivity alongside the warning icon and `.error` style, without a
    // second "is (None) selected" handler.
    let alarm_form_playing: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let alarm_form_previewing: Rc<RefCell<Option<gio::Subprocess>>> = Rc::new(RefCell::new(None));
    let preview_button = gtk4::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Preview")
        .css_classes(["flat"])
        .build();
    sound_row.add_suffix(&preview_button);

    // Flags "(None)" on the row itself: a warning glyph plus the row's
    // `.error` style (red; `.warning` is amber), updated live as the selection
    // changes. The preview button is disabled in that state, since there is
    // nothing to preview.
    //
    // The glyph is part of the title string because `title` on
    // `AdwActionRow`/`AdwComboRow` is a plain string with no slot for a widget
    // inline with the text. Prefix and suffix widgets land before the title
    // column or after the value, and a prefix icon reads as before the label.
    // The cost is a Unicode glyph instead of a themed symbolic icon.
    const SOUND_WARNING_GLYPH: &str = " \u{26a0}";
    let update_sound_warning = {
        let sound_row = sound_row.clone();
        let preview_button = preview_button.clone();
        move || {
            let is_none = sound_row.selected() == 0;
            sound_row.set_title(&if is_none {
                format!("\u{a0}\u{a0}\u{a0}\u{a0}Sound{SOUND_WARNING_GLYPH}")
            } else {
                "\u{a0}\u{a0}\u{a0}\u{a0}Sound".to_string()
            });
            if is_none {
                sound_row.add_css_class("error");
            } else {
                sound_row.remove_css_class("error");
            }
            preview_button.set_sensitive(!is_none);
        }
    };
    sound_row.connect_selected_notify({
        let update_sound_warning = update_sound_warning.clone();
        move |_| update_sound_warning()
    });

    let refresh_sound_model = {
        let sound_paths = sound_paths.clone();
        let sound_row = sound_row.clone();
        move || {
            // The current selection is captured before rebuilding, by resolved
            // file path (not index, since the model's order can change).
            // Swapping a `GtkStringList` model resets the row's selected index
            // to 0, which would otherwise reset the form's sound to "(None)"
            // as soon as the Sound Manager's first refresh runs.
            let currently_selected = sound_paths
                .borrow()
                .get(sound_row.selected() as usize)
                .cloned()
                .flatten();

            // "(None)" is always the first option: an explicit choice for an
            // alarm that rings without sound (vibration, glow or notification
            // only).
            let mut labels: Vec<String> = vec!["(None)".to_string()];
            let mut paths: Vec<Option<String>> = vec![None];
            if let Ok(library) = cajole::sound_library::SoundLibrary::load(sound_library_path()) {
                for entry in library.sorted_visible_entries().into_iter().filter(|e| e.file_path.is_some()) {
                    labels.push(entry.label.clone());
                    paths.push(entry.file_path.clone());
                }
            }
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let model = gtk4::StringList::new(&label_refs);
            sound_row.set_model(Some(&model));

            // Updated before `set_selected` so the `selected-notify` handlers
            // (warning icon, preview button) see the new paths, not the
            // pre-rebuild ones.
            let restored_index = paths.iter().position(|p| *p == currently_selected).unwrap_or(0);
            *sound_paths.borrow_mut() = paths;
            sound_row.set_selected(restored_index as u32);
        }
    };
    refresh_sound_model();
    // A new alarm gets a random sound from those available instead of
    // "(None)". There is no single-sound special case, since a random pick
    // from one entry returns that entry. An existing alarm restores its own
    // sound (matched by resolved file path, since the order can change),
    // falling back to "(None)" if that sound was removed.
    //
    // `std::time`-based pseudo-randomness avoids the `rand` crate; this is a
    // UI default with no security requirement.
    let selected_index = if is_new {
        let available_count = sound_paths.borrow().len() - 1; // excludes "(None)" at index 0
        if available_count == 0 {
            0
        } else {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            1 + (nanos as usize % available_count)
        }
    } else {
        sound_paths
            .borrow()
            .iter()
            .position(|p| *p == base.sound_file)
            .unwrap_or(0)
    };
    sound_row.set_selected(selected_index as u32);
    update_sound_warning();

    // Previews the selected sound, resolved from `sound_paths`. Added before
    // the gear button so it sits between the combo's value and "Manage
    // Sounds…".
    preview_button.connect_clicked({
        let sound_row = sound_row.clone();
        let sound_paths = sound_paths.clone();
        let playing = alarm_form_playing.clone();
        let previewing = alarm_form_previewing.clone();
        let preview_button = preview_button.clone();
        move |_| {
            if playing.get() {
                if let Some(subprocess) = previewing.borrow_mut().take() {
                    subprocess.force_exit();
                }
                playing.set(false);
                preview_button.set_icon_name("media-playback-start-symbolic");
                preview_button.set_tooltip_text(Some("Preview"));
                return;
            }

            let resolved = sound_paths
                .borrow()
                .get(sound_row.selected() as usize)
                .cloned()
                .flatten();
            let Some(resolved) = resolved else { return };

            match play_sound_preview(&resolved) {
                Ok(subprocess) => {
                    playing.set(true);
                    preview_button.set_icon_name("media-playback-stop-symbolic");
                    preview_button.set_tooltip_text(Some("Stop"));

                    let watch_subprocess = subprocess.clone();
                    let playing_watch = playing.clone();
                    let preview_button_watch = preview_button.clone();
                    let previewing_watch = previewing.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = watch_subprocess.wait_check_future().await;
                        if playing_watch.get() {
                            playing_watch.set(false);
                            preview_button_watch.set_icon_name("media-playback-start-symbolic");
                            preview_button_watch.set_tooltip_text(Some("Preview"));
                            previewing_watch.borrow_mut().take();
                        }
                    });

                    *previewing.borrow_mut() = Some(subprocess);
                }
                Err(err) => tracing::warn!(%err, "failed to preview sound"),
            }
        }
    });

    // A gear button on the picker itself, so sound management is reachable
    // without knowing about the main window's menu. Returning from it
    // refreshes this row's model in place, so a just-added or downloaded sound
    // shows up immediately.
    let manage_sounds_button = gtk4::Button::builder()
        .icon_name("emblem-system-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Manage Sounds…")
        .css_classes(["flat"])
        .build();
    sound_row.add_suffix(&manage_sounds_button);
    manage_sounds_button.connect_clicked({
        let parent = parent.clone();
        let store_path = store_path.clone();
        let refresh_sound_model = refresh_sound_model.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            show_sound_manager(&parent, store_path.clone(), toast_overlay.clone(), refresh_sound_model.clone());
        }
    });

    // Play on (destination). "Active Sink" and "All Sinks" come first, then
    // every available destination: plain sinks (Bluetooth and Null Output are
    // filtered out at the source) and, for hardware with several routes on one
    // sink, each route individually (earpiece vs. speaker). The destination is
    // per alarm.
    let play_on_options: Rc<Vec<OutputMode>> = Rc::new({
        let mut options = vec![OutputMode::ActiveSink, OutputMode::AllSinks];
        for info in cajole::settings::discover_destinations() {
            options.push(OutputMode::Specific(info.destination));
        }
        options
    });
    let play_on_labels: Vec<String> = {
        let mut labels = vec!["Active Sink".to_string(), "All Sinks".to_string()];
        for info in cajole::settings::discover_destinations() {
            labels.push(if info.is_internal {
                format!("{} (built-in)", info.description)
            } else {
                info.description.clone()
            });
        }
        labels
    };
    let play_on_label_refs: Vec<&str> = play_on_labels.iter().map(String::as_str).collect();
    let play_on_row = adw::ComboRow::builder()
        .title("\u{a0}\u{a0}\u{a0}\u{a0}Play on")
        .model(&gtk4::StringList::new(&play_on_label_refs))
        .build();
    play_on_row.set_selected(
        play_on_options
            .iter()
            .position(|m| *m == base.output_mode)
            .unwrap_or(0) as u32,
    );

    let volume_start_row = adw::SpinRow::builder()
        .title("\u{a0}\u{a0}\u{a0}\u{a0}Initial volume (%)")
        .adjustment(&gtk4::Adjustment::new(
            base.volume_start_percent as f64,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&volume_start_row);
    let volume_end_row = adw::SpinRow::builder()
        .title("\u{a0}\u{a0}\u{a0}\u{a0}Final volume (%)")
        .adjustment(&gtk4::Adjustment::new(
            base.volume_end_percent as f64,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&volume_end_row);

    // Added to the same `general_group` as above; everything from "Play sound"
    // onward continues the same list.
    general_group.add(&play_sound_row);
    general_group.add(&sound_row);
    general_group.add(&play_on_row);
    general_group.add(&volume_start_row);
    general_group.add(&volume_end_row);

    // Enable vibration
    let vibration_enabled_row = adw::SwitchRow::builder()
        .title("Enable vibration")
        .active(base.vibration_enabled)
        .build();

    const PATTERN_VALUES: [VibrationPattern; 4] = [
        VibrationPattern::RapidFire,
        VibrationPattern::SlowRoll,
        VibrationPattern::Heartbeat,
        VibrationPattern::Constant,
    ];
    let pattern_labels = ["Rapid Fire", "Slow Roll", "Heartbeat", "Constant"];
    let pattern_row = adw::ComboRow::builder()
        .title("\u{a0}\u{a0}\u{a0}\u{a0}Pattern")
        .model(&gtk4::StringList::new(&pattern_labels))
        .build();
    pattern_row.set_selected(
        PATTERN_VALUES
            .iter()
            .position(|p| *p == base.vibration_pattern)
            .unwrap_or(3) as u32,
    );

    // `play_segments` sends one `Vibrate` call per "on" segment (required on
    // real hardware, see its doc comment) and checks `should_continue` between
    // segments, so Stop can interrupt mid-pattern.
    let vibration_preview_playing: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let vibration_preview_continue: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let vibration_preview_button = gtk4::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Preview")
        .css_classes(["flat"])
        .build();
    pattern_row.add_suffix(&vibration_preview_button);
    vibration_preview_button.connect_clicked({
        let pattern_row = pattern_row.clone();
        let playing = vibration_preview_playing.clone();
        let should_continue = vibration_preview_continue.clone();
        let button = vibration_preview_button.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            if playing.get() {
                should_continue.set(false);
                playing.set(false);
                button.set_icon_name("media-playback-start-symbolic");
                button.set_tooltip_text(Some("Preview"));
                return;
            }
            let pattern = PATTERN_VALUES[pattern_row.selected() as usize];
            playing.set(true);
            should_continue.set(true);
            button.set_icon_name("media-playback-stop-symbolic");
            button.set_tooltip_text(Some("Stop"));

            let playing = playing.clone();
            let should_continue = should_continue.clone();
            let button = button.clone();
            let toast_overlay = toast_overlay.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await {
                    let allowed = cajole::settings::AppSettings::load(settings_path())
                        .unwrap_or_default()
                        .vibrate_in_silent;
                    let _silent_override = cajole::silent_override::engage(&conn, allowed);
                    if _silent_override.is_none()
                        && cajole::silent_override::current_profile(&conn).as_deref()
                            == Some("silent")
                    {
                        toast_overlay.add_toast(adw::Toast::new(
                            "Vibration is off while the phone is in Silent mode",
                        ));
                    } else {
                        let segments = cajole::haptic::build_preview_segments(pattern);
                        cajole::haptic::play_segments(&conn, &segments, &should_continue).await;
                    }
                } else {
                    tracing::warn!("failed to connect to session bus for vibration preview");
                }
                if playing.get() {
                    playing.set(false);
                    button.set_icon_name("media-playback-start-symbolic");
                    button.set_tooltip_text(Some("Preview"));
                }
            });
        }
    });

    let vibration_start_row = adw::SpinRow::builder()
        .title("\u{a0}\u{a0}\u{a0}\u{a0}Initial intensity (%)")
        .adjustment(&gtk4::Adjustment::new(
            base.vibration_start_percent as f64,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&vibration_start_row);
    let vibration_end_row = adw::SpinRow::builder()
        .title("\u{a0}\u{a0}\u{a0}\u{a0}Final intensity (%)")
        .adjustment(&gtk4::Adjustment::new(
            base.vibration_end_percent as f64,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&vibration_end_row);

    // Appended to `general_group`, like the glow row below, and only when this
    // system can vibrate.
    if vibration_available.get() {
        general_group.add(&vibration_enabled_row);
        general_group.add(&pattern_row);
        general_group.add(&vibration_start_row);
        general_group.add(&vibration_end_row);
    }

    // Enable screen glow
    let glow_enabled_row = adw::SwitchRow::builder()
        .title("Enable screen glow")
        .active(base.glow_enabled)
        .build();
    let glow_start_row = adw::SpinRow::builder()
        .title("\u{a0}\u{a0}\u{a0}\u{a0}Initial brightness (%)")
        .adjustment(&gtk4::Adjustment::new(
            base.glow_start_percent as f64,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&glow_start_row);
    let glow_end_row = adw::SpinRow::builder()
        .title("\u{a0}\u{a0}\u{a0}\u{a0}Final brightness (%)")
        .adjustment(&gtk4::Adjustment::new(
            base.glow_end_percent as f64,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&glow_end_row);

    // The vibration and glow rows appear only when this system supports them,
    // since a control that can do nothing is worse than none. The widgets
    // still exist when hidden, so values from an existing alarm are preserved
    // and read on save.
    if brightness_available.get() {
        general_group.add(&glow_enabled_row);
        general_group.add(&glow_start_row);
        general_group.add(&glow_end_row);
    }

    // One function drives every show/hide and every "Initial"/"Final" to
    // plain-label rename in the form. All of it depends on three booleans
    // (gentle wake and each section's top-level toggle), so the rules live in
    // one place instead of several handlers that would need to stay in sync.
    let update_form_visibility = {
        let gentle_wake_enabled_row = gentle_wake_enabled_row.clone();
        let gentle_wake_duration_row = gentle_wake_duration_row.clone();
        let play_sound_row = play_sound_row.clone();
        let sound_row = sound_row.clone();
        let play_on_row = play_on_row.clone();
        let volume_start_row = volume_start_row.clone();
        let volume_end_row = volume_end_row.clone();
        let vibration_enabled_row = vibration_enabled_row.clone();
        let pattern_row = pattern_row.clone();
        let vibration_start_row = vibration_start_row.clone();
        let vibration_end_row = vibration_end_row.clone();
        let glow_enabled_row = glow_enabled_row.clone();
        let glow_start_row = glow_start_row.clone();
        let glow_end_row = glow_end_row.clone();
        let brightness_available = brightness_available.clone();
        let vibration_available = vibration_available.clone();
        move || {
            let gw_on = gentle_wake_enabled_row.is_active();
            gentle_wake_duration_row.set_visible(gw_on);

            let sound_on = play_sound_row.is_active();
            sound_row.set_visible(sound_on);
            play_on_row.set_visible(sound_on);
            volume_start_row.set_visible(sound_on && gw_on);
            volume_end_row.set_visible(sound_on);
            volume_end_row.set_title(if gw_on {
                "\u{a0}\u{a0}\u{a0}\u{a0}Final volume (%)"
            } else {
                "\u{a0}\u{a0}\u{a0}\u{a0}Volume (%)"
            });

            if vibration_available.get() {
                let vib_on = vibration_enabled_row.is_active();
                pattern_row.set_visible(vib_on);
                vibration_start_row.set_visible(vib_on && gw_on);
                vibration_end_row.set_visible(vib_on);
                vibration_end_row.set_title(if gw_on {
                    "\u{a0}\u{a0}\u{a0}\u{a0}Final intensity (%)"
                } else {
                    "\u{a0}\u{a0}\u{a0}\u{a0}Intensity (%)"
                });
            }

            if brightness_available.get() {
                let glow_on = glow_enabled_row.is_active();
                glow_start_row.set_visible(glow_on && gw_on);
                glow_end_row.set_visible(glow_on);
                glow_end_row.set_title(if gw_on {
                    "\u{a0}\u{a0}\u{a0}\u{a0}Final brightness (%)"
                } else {
                    "\u{a0}\u{a0}\u{a0}\u{a0}Brightness (%)"
                });
            }
        }
    };
    update_form_visibility();
    for row in [
        &gentle_wake_enabled_row,
        &play_sound_row,
        &vibration_enabled_row,
        &glow_enabled_row,
    ] {
        row.connect_active_notify({
            let update_form_visibility = update_form_visibility.clone();
            move |_| update_form_visibility()
        });
    }

    let snooze_row = adw::SpinRow::builder()
        .title("Snooze duration (minutes)")
        .adjustment(&gtk4::Adjustment::new(
            base.snooze_minutes as f64,
            1.0,
            60.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&snooze_row);

    let early_dismissal_row = adw::SpinRow::builder()
        .title("Early dismissal window (minutes)")
        .subtitle("0 disables early dismissal")
        .adjustment(&gtk4::Adjustment::new(
            base.early_dismissal_window_minutes as f64,
            0.0,
            120.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    set_spin_row_numeric_input(&early_dismissal_row);
    general_group.add(&snooze_row);
    general_group.add(&early_dismissal_row);

    // The default `AdwPreferencesGroup` spacing (18px) applies here,
    // separating the time/day pickers from the rest of the form.
    let body = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(18)
        .build();
    body.append(&time_group);
    body.append(&repeat_group);
    body.append(&general_group);
    let clamp = adw::Clamp::builder()
        .child(&body)
        .maximum_size(480)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();
    let scrolled = gtk4::ScrolledWindow::builder().child(&clamp).vexpand(true).build();

    let cancel_button = gtk4::Button::with_label("Cancel");
    let save_button = gtk4::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .build();

    let header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new(
            if is_new { "New Alarm" } else { "Edit Alarm" },
            "",
        ))
        .build();
    header.pack_start(&cancel_button);
    header.pack_end(&save_button);

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    content.append(&scrolled);

    let toolbar_view = adw::ToolbarView::builder().content(&content).build();
    toolbar_view.add_top_bar(&header);
    if let Some(banner) = &holiday_banner {
        toolbar_view.add_top_bar(banner);
    }

    if !is_new {
        let delete_button = gtk4::Button::builder()
            .label("Delete Alarm")
            // Matches GNOME Clocks' "Remove Alarm" row: `flat` +
            // `destructive-action` renders red text on the default background,
            // deliberately milder than the confirmed state below, since this
            // is only the first of two presses.
            .css_classes(["destructive-action", "flat"])
            .build();
        let bottom_bar = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .halign(gtk4::Align::Fill)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        delete_button.set_hexpand(true);
        bottom_bar.append(&delete_button);
        // A bottom bar instead of content inside the scroll area. This pins it
        // to the bottom regardless of the form's height; `vexpand` on the
        // scrolled area did not reliably do so, because the content box was
        // not always allocated the full window height.
        toolbar_view.add_bottom_bar(&bottom_bar);

        // Two presses, in place instead of a modal dialog: the first arms it
        // (relabels to "Confirm Delete" with solid-red styling), the second
        // while armed deletes.
        let delete_armed = Rc::new(std::cell::Cell::new(false));
        delete_button.connect_clicked({
            let store_path = store_path.clone();
            let refresh_list = refresh_list.clone();
            let delete_armed = delete_armed.clone();
            let scheduler_handle_cell = scheduler_handle_cell.clone();
            move |button| {
                if !delete_armed.get() {
                    delete_armed.set(true);
                    button.set_label("Confirm Delete");
                    button.remove_css_class("flat");
                    return;
                }

                match AlarmStore::load(&store_path) {
                    Ok(mut store) => {
                        if let Err(err) = store.remove(alarm_id) {
                            tracing::warn!(%err, "failed to delete alarm");
                        }
                    }
                    Err(err) => tracing::error!(%err, "failed to load alarm store to delete alarm"),
                }
                request_reschedule(&scheduler_handle_cell);
                refresh_list();
                if let Some(window) = button.root().and_then(|r| r.downcast::<adw::Window>().ok()) {
                    window.close();
                }
            }
        });
    }

    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .default_height(680)
        .content(&toolbar_view)
        .build();

    cancel_button.connect_clicked({
        let dialog = dialog.clone();
        move |_| dialog.close()
    });

    save_button.connect_clicked({
        let dialog = dialog.clone();
        let store_path = store_path.clone();
        let refresh_list = refresh_list.clone();
        let toast_overlay = toast_overlay.clone();
        let tracker = tracker.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move |_| {
            let is_pm = time_picker.am_pm.as_ref().map(|t| t.is_pm()).unwrap_or(false);
            let hour = hour_24_from_form(time_picker.hour.value() as u32, is_pm, use_12h);
            let minute = time_picker.minute.value() as u32;
            let time = NaiveTime::from_hms_opt(hour, minute, 0).unwrap_or(base.time);

            let alarm = Alarm {
                id: alarm_id,
                // No fallback text: a blank label is valid, as in GNOME
                // Clocks' optional "Name" field.
                label: label_row.text().to_string(),
                time,
                repeat: collect_repeat_days(&day_toggles),
                // Not a form field: "Enabled" is an inline switch on the main
                // list, as in GNOME Clocks. The existing value is preserved on
                // edit.
                enabled: base.enabled,
                gentle_wake_enabled: gentle_wake_enabled_row.is_active(),
                gentle_wake_duration_secs: gentle_wake_duration_row.value() as u32,
                play_sound: play_sound_row.is_active(),
                sound_file: sound_paths
                    .borrow()
                    .get(sound_row.selected() as usize)
                    .cloned()
                    .flatten(),
                output_mode: play_on_options
                    .get(play_on_row.selected() as usize)
                    .cloned()
                    .unwrap_or(OutputMode::ActiveSink),
                volume_start_percent: volume_start_row.value() as u8,
                volume_end_percent: volume_end_row.value() as u8,
                vibration_enabled: vibration_enabled_row.is_active(),
                vibration_pattern: PATTERN_VALUES[pattern_row.selected() as usize],
                vibration_start_percent: vibration_start_row.value() as u8,
                vibration_end_percent: vibration_end_row.value() as u8,
                glow_enabled: glow_enabled_row.is_active(),
                glow_start_percent: glow_start_row.value() as u8,
                glow_end_percent: glow_end_row.value() as u8,
                alarm_duration_minutes: alarm_duration_row.value() as u32,
                snooze_minutes: snooze_row.value() as u32,
                early_dismissal_window_minutes: early_dismissal_row.value() as u32,
                // Not a form field: the Snooze button on the ring screen sets
                // it. Preserved so editing an alarm mid-snooze does not cancel
                // the snooze.
                snoozed_until: base.snoozed_until,
                // Preserved too. Unlike `snoozed_until`, this is a bare date
                // ("skip any occurrence on or before"), which stays correct
                // whatever the schedule is edited to.
                skip_through: base.skip_through,
                // Neither is a form field. `holiday_excluded` is set only from
                // the Holiday Mode wizard's alarm list, and `holiday_only`
                // only when the form was opened from the wizard's "+ Holiday
                // Alarm" flow (see `holiday_only_override` below).
                holiday_excluded: base.holiday_excluded,
                holiday_only: holiday_only_override.unwrap_or(base.holiday_only),
            };

            match AlarmStore::load(&store_path) {
                Ok(mut store) => {
                    if let Err(err) = store.upsert(alarm.clone()) {
                        tracing::error!(%err, "failed to save alarm");
                    } else {
                        if let Some(on_saved) = &on_saved {
                            on_saved(alarm.id);
                        }
                        // The scheduler's cached next trigger for this alarm
                        // must be dropped: `TriggerTracker::sync` computes a
                        // fresh value only for an alarm it is not already
                        // tracking, so an edited time or repeat (or a
                        // preserved `skip_through`/`snoozed_until`) would not
                        // take effect until something else invalidated the
                        // cache. Same reasoning as `scheduler::snooze` and the
                        // `dismiss-alarm-early` action's `forget` calls.
                        tracker.borrow_mut().forget(alarm.id);
                        request_reschedule(&scheduler_handle_cell);
                        if let Some(next) = cajole::next_trigger(&alarm, chrono::Local::now()) {
                            // Matches GNOME Clocks' "Alarm set for X from now"
                            // toast: shown only for an enabled alarm with a
                            // next ring time (`next_trigger` returns `None`
                            // for a disabled one).
                            let when = cajole::schedule::format_time_until(chrono::Local::now(), next);
                            toast_overlay.add_toast(adw::Toast::new(&format!("Alarm set for {when} from now")));
                        }
                    }
                }
                Err(err) => tracing::error!(%err, "failed to load alarm store to save alarm"),
            }
            dialog.close();
            // Deferred to the next event-loop turn. Running this form's
            // teardown and the caller's refresh (two list rebuilds when
            // reached via "+ Holiday Alarm") in the same GTK frame caused a
            // reproducible freeze on phone hardware elsewhere in this wizard,
            // and this save path had not received that fix.
            let refresh_list = refresh_list.clone();
            glib::idle_add_local_once(move || {
                refresh_list();
            });
        }
    });

    dialog.present();
}

/// Handles an alarm being dismissed, whether from the ring dialog's Dismiss
/// button or a notification's Dismiss action.
async fn handle_alarm_dismissed(
    app: &adw::Application,
    store_path: &std::path::Path,
    ringing: &RingingMap,
    open_ring_dialogs: &RingDialogs,
    refresh_list: &RefreshFn,
    scheduler_handle: Option<scheduler::SchedulerHandle>,
    alarm_id: Uuid,
) {
    app.withdraw_notification(&alarm_id.to_string());
    scheduler::dismiss(store_path, ringing, alarm_id).await;
    if let Some(handle) = scheduler_handle {
        scheduler::check_and_reschedule(&handle).await;
    }
    if let Some(dialog) = open_ring_dialogs.borrow_mut().remove(&alarm_id) {
        dialog.close();
    }
    refresh_list();
}

/// Handles an alarm being snoozed, whether from the ring dialog's Snooze
/// button or a notification's Snooze action.
async fn handle_alarm_snoozed(
    app: &adw::Application,
    store_path: &std::path::Path,
    ringing: &RingingMap,
    tracker: &SharedTracker,
    open_ring_dialogs: &RingDialogs,
    refresh_list: &RefreshFn,
    scheduler_handle: Option<scheduler::SchedulerHandle>,
    alarm_id: Uuid,
) {
    app.withdraw_notification(&alarm_id.to_string());
    scheduler::snooze(store_path, ringing, tracker, alarm_id).await;
    if let Some(handle) = scheduler_handle {
        scheduler::check_and_reschedule(&handle).await;
    }
    if let Some(dialog) = open_ring_dialogs.borrow_mut().remove(&alarm_id) {
        dialog.close();
    }
    refresh_list();
}


/// The ring notification is a system notification with Snooze/Dismiss actions
/// (see `SchedulerEvent::Ringing`'s handler in `build_ui`), which works from a
/// locked phone. This dialog is secondary, for when Cajole is already in the
/// foreground.
fn show_ring_dialog(
    parent: &adw::ApplicationWindow,
    app: &adw::Application,
    alarm_id: Uuid,
    label: &str,
    store_path: PathBuf,
    ringing: RingingMap,
    tracker: SharedTracker,
    open_ring_dialogs: RingDialogs,
    refresh_list: RefreshFn,
    scheduler_handle_cell: Rc<RefCell<Option<scheduler::SchedulerHandle>>>,
) {
    let status = adw::StatusPage::builder()
        .title(label)
        .icon_name("net.dunevent.Cajole-symbolic")
        .build();

    let snooze_button = gtk4::Button::builder().label("Snooze").css_classes(["pill"]).build();
    let dismiss_button = gtk4::Button::builder()
        .label("Dismiss")
        .css_classes(["pill", "suggested-action"])
        .build();

    let button_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk4::Align::Center)
        .margin_bottom(24)
        .build();
    button_box.append(&snooze_button);
    button_box.append(&dismiss_button);

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .vexpand(true)
        .build();
    content.append(&status);
    content.append(&button_box);

    // Not the alarm's UI on a locked phone: see `SchedulerEvent::Ringing`'s
    // handler, which sends a system notification with Snooze/Stop actions that
    // works with no window open, including from the lock screen. The gentle
    // wake glow is a hardware brightness ramp (see `ring.rs` and
    // `display::set_screen_brightness`), not drawn here, so it is visible
    // above the lock screen.
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(360)
        .default_height(640)
        .content(&content)
        .build();


    open_ring_dialogs.borrow_mut().insert(alarm_id, dialog.clone());

    dismiss_button.connect_clicked({
        let app = app.clone();
        let store_path = store_path.clone();
        let ringing = ringing.clone();
        let open_ring_dialogs = open_ring_dialogs.clone();
        let refresh_list = refresh_list.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move |_| {
            let app = app.clone();
            let store_path = store_path.clone();
            let ringing = ringing.clone();
            let open_ring_dialogs = open_ring_dialogs.clone();
            let refresh_list = refresh_list.clone();
            let scheduler_handle = scheduler_handle_cell.borrow().clone();
            glib::MainContext::default().spawn_local(async move {
                handle_alarm_dismissed(
                    &app,
                    &store_path,
                    &ringing,
                    &open_ring_dialogs,
                    &refresh_list,
                    scheduler_handle,
                    alarm_id,
                )
                .await;
            });
        }
    });

    snooze_button.connect_clicked({
        let app = app.clone();
        let ringing = ringing.clone();
        let tracker = tracker.clone();
        let open_ring_dialogs = open_ring_dialogs.clone();
        let refresh_list = refresh_list.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move |_| {
            let app = app.clone();
            let ringing = ringing.clone();
            let tracker = tracker.clone();
            let store_path = store_path.clone();
            let open_ring_dialogs = open_ring_dialogs.clone();
            let refresh_list = refresh_list.clone();
            let scheduler_handle = scheduler_handle_cell.borrow().clone();
            glib::MainContext::default().spawn_local(async move {
                handle_alarm_snoozed(
                    &app,
                    &store_path,
                    &ringing,
                    &tracker,
                    &open_ring_dialogs,
                    &refresh_list,
                    scheduler_handle,
                    alarm_id,
                )
                .await;
            });
        }
    });

    dialog.present();
}

/// PID of a running download, shared so Stop can kill it. `None` once the
/// download has finished, failed or been cancelled.
type DownloadCancelHandle = std::sync::Arc<std::sync::Mutex<Option<u32>>>;

/// Kills an in-flight download, if one is running under this handle. The
/// killed `curl` reports back through [`download_sound_async`]'s channel as a
/// failure (exit by signal), so the call sites' existing failure handling
/// reverts the row's icons and re-enables Download. There is no separate
/// cancelled path.
fn cancel_download(handle: &DownloadCancelHandle) {
    if let Some(pid) = handle.lock().unwrap().take() {
        // SAFETY: `pid` came from `Child::id()` moments ago and identifies a
        // process this app spawned. It is only signalled, never dereferenced;
        // signalling an already-exited pid is a defined error (ESRCH), not
        // undefined behavior.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// Downloads `source_url` to `dest_path` on a background OS thread by shelling
/// out to `curl`, as the app does for `pactl`, `fbcli` and `paplay`. curl uses
/// the system trust store and needs no `openssl-sys`/`native-tls` build
/// dependency. Being a subprocess, the download can be cancelled, see
/// [`cancel_download`].
///
/// `--max-time 30` makes a stalled connection fail instead of hanging.
/// `--fail` makes curl exit non-zero on an HTTP error status instead of
/// writing the error page to disk as the sound file.
///
/// Returns a receiver because the result crosses an OS thread boundary and
/// must be `Send`, which the plain `Result<(), String>` is and the caller's
/// `Rc`-based GTK state is not. The caller awaits it from a
/// `glib::MainContext::spawn_local` task (see the download button's click
/// handler), which runs on the main thread.
fn download_sound_async(
    source_url: String,
    dest_path: PathBuf,
) -> (async_channel::Receiver<Result<(), String>>, DownloadCancelHandle) {
    let (tx, rx) = async_channel::bounded(1);
    let cancel_handle: DownloadCancelHandle = std::sync::Arc::new(std::sync::Mutex::new(None));
    let cancel_handle_thread = cancel_handle.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            let child = std::process::Command::new("curl")
                .args(["--fail", "--silent", "--show-error", "--location", "--max-time", "30"])
                .arg("--output")
                .arg(&dest_path)
                .arg(&source_url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|err| format!("failed to run curl: {err}"))?;

            // Published right after spawn and cleared once curl exits. A Stop
            // click can only find and kill the process during this window.
            *cancel_handle_thread.lock().unwrap() = Some(child.id());
            let output = child
                .wait_with_output()
                .map_err(|err| format!("failed to wait for curl: {err}"))?;
            *cancel_handle_thread.lock().unwrap() = None;

            if !output.status.success() {
                // A killed or failed curl may have left a partial file at
                // `dest_path`. Best-effort removal; the file may not exist.
                let _ = std::fs::remove_file(&dest_path);
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("download failed: {}", stderr.trim()));
            }

            // `--fail` catches only HTTP-level errors. A captive portal, a
            // misdirected redirect or an HTML error page served under a 200
            // status would pass it. Every catalog sound is FLAC (GNOME Clocks)
            // or Ogg (AOSP, KDE Ocean, KDE Plasma Mobile), and both start with
            // a known magic signature, so the file's header is checked
            // directly.
            let header = std::fs::read(&dest_path)
                .map_err(|err| format!("failed to read downloaded file: {err}"))?;
            let is_flac = header.starts_with(b"fLaC");
            let is_ogg = header.starts_with(b"OggS");
            if !is_flac && !is_ogg {
                let _ = std::fs::remove_file(&dest_path);
                return Err(
                    "download did not return a recognizable audio file".to_string(),
                );
            }

            Ok(())
        })();
        let _ = tx.send_blocking(result);
    });
    (rx, cancel_handle)
}

/// Converts a `NaiveDate` to the `glib::DateTime` that `GtkCalendar` takes
/// (local midnight).
fn naive_to_glib_date(date: NaiveDate) -> glib::DateTime {
    glib::DateTime::from_local(date.year(), date.month() as i32, date.day() as i32, 0, 0, 0.0)
        .expect("a NaiveDate's own fields are always a valid GDateTime")
}

fn glib_date_to_naive(dt: &glib::DateTime) -> NaiveDate {
    NaiveDate::from_ymd_opt(dt.year(), dt.month() as u32, dt.day_of_month() as u32)
        .expect("GtkCalendar only ever selects a real day of a real month")
}

/// One of the two Holiday Mode date pickers: an `AdwActionRow` (title plus a
/// subtitle showing the staged value or "Not set") that opens a `GtkPopover`
/// containing the `GtkCalendar`, plus an `edit-clear-symbolic` suffix button.
/// A popover because two full calendars do not fit a phone-height screen
/// (confirmed on a real build), and it is the standard GNOME pattern.
struct HolidayDateRow {
    container: gtk4::Widget,
}

/// Builds one row around the caller's `staged` cell, so the wizard can read
/// both rows' values from one place to drive its Next button without either
/// row knowing the other. `min` is a closure so it reads the other row's
/// current value live; a pick below it snaps the calendar back to the nearest
/// valid date.
///
/// Only the checkmark button commits a date: it stages whatever the calendar
/// shows when pressed. `day-selected` still runs live, but only to snap an
/// out-of-range pick back to `min()`; it never stages or closes anything. Two
/// earlier designs were dropped. Committing on `day-selected` made tapping an
/// already-highlighted day do nothing, since `GtkCalendar` emits that signal
/// only on a change. `GtkCalendar`'s documented `day: 0` "unselect" state
/// would have made every first pick a change, but it panics at runtime in this
/// project's GTK4 version.
fn build_holiday_date_row(
    title: &str,
    staged: Rc<RefCell<Option<NaiveDate>>>,
    min: impl Fn() -> NaiveDate + 'static,
    on_change: Rc<dyn Fn()>,
) -> HolidayDateRow {
    let min: Rc<dyn Fn() -> NaiveDate> = Rc::new(min);
    let initial = *staged.borrow();
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(match initial {
            Some(date) => date.format("%B %-d, %Y").to_string(),
            None => "Not set".to_string(),
        })
        .activatable(true)
        .build();

    let calendar = gtk4::Calendar::builder().build();
    calendar.select_day(&naive_to_glib_date(initial.unwrap_or_else(&*min)));

    let confirm_button = gtk4::Button::builder()
        .icon_name("object-select-symbolic")
        .tooltip_text("Confirm")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    let popover_content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    let confirm_bar = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .halign(gtk4::Align::End)
        .margin_top(4)
        .build();
    confirm_bar.append(&confirm_button);
    popover_content.append(&calendar);
    popover_content.append(&confirm_bar);
    let popover = gtk4::Popover::builder().child(&popover_content).build();
    popover.set_parent(&row);

    calendar.connect_day_selected({
        let min = min.clone();
        move |cal| {
            let picked = glib_date_to_naive(&cal.date());
            let floor = min();
            if picked < floor {
                // Re-enters this handler; the corrected date passes the check
                // above, so it does not recurse further.
                cal.select_day(&naive_to_glib_date(floor));
            }
        }
    });

    confirm_button.connect_clicked({
        let calendar = calendar.clone();
        let min = min.clone();
        let staged = staged.clone();
        let row = row.clone();
        let on_change = on_change.clone();
        let popover = popover.clone();
        move |_| {
            let picked = glib_date_to_naive(&calendar.date());
            // `day-selected` already snaps out-of-range picks back, so the
            // calendar cannot show an invalid day here. The floor is kept
            // because this reads the calendar's raw state, not the handler's
            // result.
            let date = picked.max(min());
            *staged.borrow_mut() = Some(date);
            row.set_subtitle(&date.format("%B %-d, %Y").to_string());
            on_change();
            popover.popdown();
        }
    });

    let clear_button = gtk4::Button::builder()
        .icon_name("edit-clear-symbolic")
        .tooltip_text(format!("Clear {title}"))
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    clear_button.connect_clicked({
        let staged = staged.clone();
        let row = row.clone();
        let on_change = on_change.clone();
        move |_| {
            *staged.borrow_mut() = None;
            row.set_subtitle("Not set");
            on_change();
        }
    });
    row.add_suffix(&clear_button);

    row.connect_activated({
        let popover = popover.clone();
        move |_| popover.popup()
    });

    HolidayDateRow {
        container: row.upcast(),
    }
}

/// The 3-page Holiday Mode wizard: dates, then which regular alarms to
/// exclude, then holiday-only alarms. Dates and exclusions are staged in
/// memory and written on "Done" (or a "Next" that reads "Done", see
/// `on_dates_changed`); "Cancel" discards them.
///
/// "+ Holiday Alarm" is the exception: it saves through the alarm form
/// immediately, like every other alarm creation. The alarm needs the current
/// staged dates on disk as soon as it exists, or the next scheduler poll's
/// `holiday::sweep` would see no configured holiday and delete it as orphaned.
/// This is why `holiday_settings_path` is saved to twice in this function:
/// provisionally when an alarm is added, and for real on "Done". "Cancel"
/// undoes both: it deletes alarms added this session (tracked in
/// `newly_added_holiday_alarm_ids`) and restores `holiday.json` to its state
/// when the wizard opened.
#[allow(clippy::too_many_arguments)]
fn show_holiday_wizard(
    parent: &adw::ApplicationWindow,
    store_path: PathBuf,
    refresh_list: RefreshFn,
    toast_overlay: adw::ToastOverlay,
    vibration_available: Rc<Cell<bool>>,
    brightness_available: Rc<Cell<bool>>,
    tracker: SharedTracker,
    scheduler_handle_cell: Rc<RefCell<Option<scheduler::SchedulerHandle>>>,
) {
    let holiday_settings_path = holiday_path();
    let today = chrono::Local::now().date_naive();
    let existing =
        cajole::holiday::HolidaySettings::load(&holiday_settings_path).unwrap_or_default();

    let start_staged: Rc<RefCell<Option<NaiveDate>>> = Rc::new(RefCell::new(existing.start));
    let end_staged: Rc<RefCell<Option<NaiveDate>>> = Rc::new(RefCell::new(existing.end));
    // Populated once the wizard's window is built, near the end of this
    // function. Anything opened from the wizard reads this instead of
    // `parent`, so it is `transient_for` the wizard, not the main window. Two
    // sibling dialogs transient for the same ancestor were a structural
    // mismatch.
    let dialog_cell: Rc<RefCell<Option<adw::Window>>> = Rc::new(RefCell::new(None));
    // Alarms added via "+ Holiday Alarm" this session, so Cancel can delete
    // them. That flow saves through the alarm form immediately instead of
    // staging.
    let newly_added_holiday_alarm_ids: Rc<RefCell<std::collections::HashSet<Uuid>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));
    let excluded_ids: Rc<RefCell<std::collections::HashSet<Uuid>>> = Rc::new(RefCell::new(
        AlarmStore::load(&store_path)
            .map(|s| {
                s.alarms()
                    .iter()
                    .filter(|a| a.holiday_excluded)
                    .map(|a| a.id)
                    .collect()
            })
            .unwrap_or_default(),
    ));

    let stack = gtk4::Stack::new();

    let cancel_button = gtk4::Button::with_label("Cancel");
    let back_button = gtk4::Button::with_label("Back");
    let next_button = gtk4::Button::builder()
        .label("Next")
        .css_classes(["suggested-action"])
        .build();
    let done_button = gtk4::Button::builder()
        .label("Done")
        .css_classes(["suggested-action"])
        .build();

    // Page 1: dates
    let on_dates_changed: Rc<dyn Fn()> = {
        let start_staged = start_staged.clone();
        let end_staged = end_staged.clone();
        let next_button = next_button.clone();
        Rc::new(move || {
            // With either date cleared Holiday Mode is off, so the button
            // saves that and closes instead of continuing to the next page.
            let both_set = start_staged.borrow().is_some() && end_staged.borrow().is_some();
            next_button.set_label(if both_set { "Next" } else { "Done" });
        })
    };
    let start_row = build_holiday_date_row(
        "Holiday Start",
        start_staged.clone(),
        move || today,
        on_dates_changed.clone(),
    );
    let end_row = build_holiday_date_row(
        "Holiday End",
        end_staged.clone(),
        {
            let start_staged = start_staged.clone();
            move || start_staged.borrow().map(|d| d.max(today)).unwrap_or(today)
        },
        on_dates_changed.clone(),
    );
    on_dates_changed();

    let dates_explainer = gtk4::Label::builder()
        .label(
            "Holiday Mode pauses selected regular alarms and allows setting \
             temporary holiday-only ones. When the period ends, your normal \
             schedule automatically resumes.",
        )
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .margin_bottom(12)
        .build();
    let dates_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    dates_list.append(&start_row.container);
    dates_list.append(&end_row.container);
    let dates_page = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    dates_page.append(&dates_explainer);
    dates_page.append(&dates_list);
    stack.add_named(
        &gtk4::ScrolledWindow::builder()
            .child(&dates_page)
            .vexpand(true)
            .build(),
        Some("dates"),
    );

    // Page 2: exclude regular alarms
    let exclude_explainer = gtk4::Label::builder()
        .label("Select the regular alarms that should be paused for the duration of the Holiday Mode period.")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .margin_bottom(12)
        .build();
    let exclude_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    if let Ok(store) = AlarmStore::load(&store_path) {
        for alarm in store.alarms().iter().filter(|a| !a.holiday_only) {
            let title = if alarm.label.trim().is_empty() {
                "(no label)".to_string()
            } else {
                alarm.label.clone()
            };
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(alarm.time.format("%H:%M").to_string())
                .build();
            let check = gtk4::CheckButton::builder()
                .active(excluded_ids.borrow().contains(&alarm.id))
                .valign(gtk4::Align::Center)
                .build();
            row.add_prefix(&check);
            row.set_activatable_widget(Some(&check));
            let id = alarm.id;
            check.connect_toggled({
                let excluded_ids = excluded_ids.clone();
                move |check| {
                    if check.is_active() {
                        excluded_ids.borrow_mut().insert(id);
                    } else {
                        excluded_ids.borrow_mut().remove(&id);
                    }
                }
            });
            exclude_list.append(&row);
        }
    }
    let exclude_page = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    exclude_page.append(&exclude_explainer);
    exclude_page.append(&exclude_list);
    stack.add_named(
        &gtk4::ScrolledWindow::builder()
            .child(&exclude_page)
            .vexpand(true)
            .build(),
        Some("exclude"),
    );

    // Page 3: holiday-only alarms
    let holiday_alarms_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let add_holiday_alarm_button = gtk4::Button::builder()
        .label("+ Holiday Alarm")
        .halign(gtk4::Align::Center)
        .build();
    let holiday_page = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    holiday_page.append(&add_holiday_alarm_button);
    holiday_page.append(&holiday_alarms_list);
    stack.add_named(
        &gtk4::ScrolledWindow::builder()
            .child(&holiday_page)
            .vexpand(true)
            .build(),
        Some("holiday-alarms"),
    );

    // A plain `Rc<dyn Fn()>` cannot call itself, so the delete button captures
    // this shared cell and looks the closure up through it at click time, when
    // it is populated.
    type SelfRefreshCell = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
    let refresh_holiday_alarms_list_cell: SelfRefreshCell = Rc::new(RefCell::new(None));
    let refresh_holiday_alarms_list: Rc<dyn Fn()> = {
        let holiday_alarms_list = holiday_alarms_list.clone();
        let store_path = store_path.clone();
        let refresh_holiday_alarms_list_cell = refresh_holiday_alarms_list_cell.clone();
        let dialog_cell = dialog_cell.clone();
        let toast_overlay = toast_overlay.clone();
        let vibration_available = vibration_available.clone();
        let brightness_available = brightness_available.clone();
        let tracker = tracker.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        let refresh_list = refresh_list.clone();
        Rc::new(move || {
            while let Some(row) = holiday_alarms_list.row_at_index(0) {
                holiday_alarms_list.remove(&row);
            }
            let use_12h = system_uses_12h_clock();
            // `None` only if this ran before the wizard's window exists. The
            // one call site that could do that is deferred past that point
            // (see where this closure is first invoked).
            let Some(wizard_window) = dialog_cell.borrow().clone() else {
                return;
            };
            if let Ok(store) = AlarmStore::load(&store_path) {
                for alarm in store.alarms().iter().filter(|a| a.holiday_only) {
                    // Refreshes both this page and the main window behind it,
                    // as "+ Holiday Alarm" does after a save. A delete through
                    // the row's edit form is an immediate write, not staged
                    // until "Done".
                    let on_change: RefreshFn = {
                        let refresh_holiday_alarms_list_cell =
                            refresh_holiday_alarms_list_cell.clone();
                        let refresh_list = refresh_list.clone();
                        Rc::new(move || {
                            if let Some(refresh) =
                                refresh_holiday_alarms_list_cell.borrow().as_ref()
                            {
                                refresh();
                            }
                            refresh_list();
                        })
                    };
                    let row = build_alarm_row(
                        alarm,
                        use_12h,
                        false,
                        &store_path,
                        &wizard_window,
                        &toast_overlay,
                        &vibration_available,
                        &brightness_available,
                        &tracker,
                        &scheduler_handle_cell,
                        on_change,
                    );
                    holiday_alarms_list.append(&row);
                }
            }
            holiday_alarms_list.set_visible(holiday_alarms_list.row_at_index(0).is_some());
        })
    };
    *refresh_holiday_alarms_list_cell.borrow_mut() = Some(refresh_holiday_alarms_list.clone());

    add_holiday_alarm_button.connect_clicked({
        let dialog_cell = dialog_cell.clone();
        let store_path = store_path.clone();
        let refresh_list = refresh_list.clone();
        let toast_overlay = toast_overlay.clone();
        let vibration_available = vibration_available.clone();
        let brightness_available = brightness_available.clone();
        let tracker = tracker.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        let start_staged = start_staged.clone();
        let end_staged = end_staged.clone();
        let refresh_holiday_alarms_list = refresh_holiday_alarms_list.clone();
        let holiday_settings_path = holiday_settings_path.clone();
        let newly_added_holiday_alarm_ids = newly_added_holiday_alarm_ids.clone();
        move |_| {
            // Both are `Some` here: this page is reachable only after page 1's
            // "Next", which stays insensitive until both dates are set.
            let (Some(start), Some(end)) = (*start_staged.borrow(), *end_staged.borrow()) else {
                return;
            };
            // Provisional write. The new alarm needs the dates on disk now, or
            // the next scheduler poll's `holiday::sweep` would see no
            // configured holiday and delete it as orphaned before the wizard
            // finishes. "Cancel" restores the previous contents.
            let settings = cajole::holiday::HolidaySettings {
                start: Some(start),
                end: Some(end),
            };
            if let Err(err) = settings.save(&holiday_settings_path) {
                tracing::warn!(%err, "failed to provisionally save holiday dates");
            }
            let refresh_holiday_alarms_list = refresh_holiday_alarms_list.clone();
            let combined_refresh: RefreshFn = {
                let refresh_list = refresh_list.clone();
                Rc::new(move || {
                    refresh_list();
                    refresh_holiday_alarms_list();
                })
            };
            let newly_added_holiday_alarm_ids = newly_added_holiday_alarm_ids.clone();
            // Always `Some` when the button can be clicked: the wizard's
            // window is built before `present()`.
            let Some(wizard_window) = dialog_cell.borrow().clone() else {
                return;
            };
            show_alarm_form(
                &wizard_window,
                None,
                store_path.clone(),
                combined_refresh,
                toast_overlay.clone(),
                vibration_available.clone(),
                brightness_available.clone(),
                tracker.clone(),
                scheduler_handle_cell.clone(),
                Some((start, end)),
                Some(Rc::new(move |id: Uuid| {
                    newly_added_holiday_alarm_ids.borrow_mut().insert(id);
                })),
            );
        }
    });

    // Assembly
    let bottom_bar = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    bottom_bar.append(&cancel_button);
    bottom_bar.append(&spacer);
    bottom_bar.append(&back_button);
    bottom_bar.append(&next_button);
    bottom_bar.append(&done_button);

    let header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new("Holiday Mode", ""))
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    content.append(&stack);

    let toolbar_view = adw::ToolbarView::builder().content(&content).build();
    toolbar_view.add_top_bar(&header);
    toolbar_view.add_bottom_bar(&bottom_bar);

    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .default_height(620)
        .content(&toolbar_view)
        .build();
    *dialog_cell.borrow_mut() = Some(dialog.clone());
    // Deferred from the closure's definition above: it resolves `dialog_cell`
    // on every call, so the first call must wait until the wizard's window
    // exists.
    refresh_holiday_alarms_list();

    let update_nav_for_page = {
        let back_button = back_button.clone();
        let next_button = next_button.clone();
        let done_button = done_button.clone();
        move |page: &str| match page {
            "exclude" => {
                back_button.set_visible(true);
                next_button.set_visible(true);
                done_button.set_visible(false);
            }
            "holiday-alarms" => {
                back_button.set_visible(true);
                next_button.set_visible(false);
                done_button.set_visible(true);
            }
            _ => {
                back_button.set_visible(false);
                next_button.set_visible(true);
                done_button.set_visible(false);
            }
        }
    };
    update_nav_for_page("dates");

    let commit_and_close: Rc<dyn Fn()> = {
        let dialog = dialog.clone();
        let refresh_list = refresh_list.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        let store_path = store_path.clone();
        let holiday_settings_path = holiday_settings_path.clone();
        let start_staged = start_staged.clone();
        let end_staged = end_staged.clone();
        let excluded_ids = excluded_ids.clone();
        Rc::new(move || {
            tracing::debug!("holiday wizard: Done clicked, starting commit");
            let settings = cajole::holiday::HolidaySettings {
                start: *start_staged.borrow(),
                end: *end_staged.borrow(),
            };
            if let Err(err) = settings.save(&holiday_settings_path) {
                tracing::warn!(%err, "failed to save holiday dates");
            }
            tracing::debug!("holiday wizard: dates saved");
            if let Ok(mut store) = AlarmStore::load(&store_path) {
                let excluded = excluded_ids.borrow().clone();
                for alarm in store.alarms().to_vec() {
                    let should_be = excluded.contains(&alarm.id);
                    if alarm.holiday_excluded != should_be {
                        let mut updated = alarm.clone();
                        updated.holiday_excluded = should_be;
                        if let Err(err) = store.upsert(updated) {
                            tracing::warn!(%err, "failed to save holiday exclusion");
                        }
                    }
                }
                tracing::debug!("holiday wizard: exclusions saved");
                // Dates may have been narrowed after a holiday alarm was
                // added; anything left with nothing to ring for is removed
                // now.
                if let Err(err) =
                    cajole::holiday::prune_exhausted(&mut store, &settings, chrono::Local::now())
                {
                    tracing::warn!(%err, "failed to prune exhausted holiday alarms");
                }
                tracing::debug!("holiday wizard: exhausted holiday alarms pruned");
            }
            request_reschedule(&scheduler_handle_cell);
            tracing::debug!("holiday wizard: reschedule requested (async, not awaited here)");
            dialog.close();
            tracing::debug!("holiday wizard: dialog closed, deferring main window refresh");
            // Deferred to the next event-loop turn. Closing the wizard tears
            // down three pages of widgets and closures; doing that in the same
            // GTK frame as rebuilding the main window's list (which may add a
            // section) is the suspected cause of a freeze seen on phone
            // hardware, never in a sandboxed test. Separating the two is an
            // untested mitigation, not a verified fix.
            let refresh_list = refresh_list.clone();
            glib::idle_add_local_once(move || {
                tracing::debug!("holiday wizard: running deferred main window refresh");
                refresh_list();
                tracing::debug!("holiday wizard: deferred main window refresh done");
            });
        })
    };

    // "Next" and "Back" only navigate and write nothing, except the early exit
    // where "Next" reads "Done" (a date was cleared on page 1), which commits
    // like "Done".
    next_button.connect_clicked({
        let stack = stack.clone();
        let update_nav_for_page = update_nav_for_page.clone();
        let refresh_holiday_alarms_list = refresh_holiday_alarms_list.clone();
        let start_staged = start_staged.clone();
        let end_staged = end_staged.clone();
        let commit_and_close = commit_and_close.clone();
        move |_| match stack.visible_child_name().as_deref() {
            Some("dates") => {
                let both_set = start_staged.borrow().is_some() && end_staged.borrow().is_some();
                if !both_set {
                    commit_and_close();
                    return;
                }
                stack.set_visible_child_name("exclude");
                update_nav_for_page("exclude");
            }
            Some("exclude") => {
                refresh_holiday_alarms_list();
                stack.set_visible_child_name("holiday-alarms");
                update_nav_for_page("holiday-alarms");
            }
            _ => {}
        }
    });

    back_button.connect_clicked({
        let stack = stack.clone();
        let update_nav_for_page = update_nav_for_page.clone();
        move |_| match stack.visible_child_name().as_deref() {
            Some("exclude") => {
                stack.set_visible_child_name("dates");
                update_nav_for_page("dates");
            }
            Some("holiday-alarms") => {
                stack.set_visible_child_name("exclude");
                update_nav_for_page("exclude");
            }
            _ => {}
        }
    });

    // Undoes everything staged this session. Alarms added via "+ Holiday
    // Alarm" (tracked in `newly_added_holiday_alarm_ids`; that flow saves
    // through the alarm form immediately) are deleted, and `holiday.json` is
    // restored to its contents when the wizard opened, undoing the provisional
    // write that kept a new alarm alive through [`holiday::sweep`].
    cancel_button.connect_clicked({
        let dialog = dialog.clone();
        let store_path = store_path.clone();
        let holiday_settings_path = holiday_settings_path.clone();
        let newly_added_holiday_alarm_ids = newly_added_holiday_alarm_ids.clone();
        let original_settings = existing.clone();
        let refresh_list = refresh_list.clone();
        let scheduler_handle_cell = scheduler_handle_cell.clone();
        move |_| {
            let mut deleted_any = false;
            if let Ok(mut store) = AlarmStore::load(&store_path) {
                for id in newly_added_holiday_alarm_ids.borrow().iter() {
                    match store.remove(*id) {
                        Ok(()) => deleted_any = true,
                        Err(err) => {
                            tracing::warn!(%err, "failed to remove a cancelled holiday alarm")
                        }
                    }
                }
            }
            if let Err(err) = original_settings.save(&holiday_settings_path) {
                tracing::warn!(%err, "failed to restore holiday dates on cancel");
            }
            // "+ Holiday Alarm" already refreshed the main list. Undoing it
            // needs the same refresh, or the deleted row and the scheduler's
            // cached next trigger for it would remain until a later refresh.
            dialog.close();
            if deleted_any {
                // Deferred to the next event-loop turn, for the same reason as
                // in `commit_and_close`: closing the wizard and rebuilding the
                // main list in one GTK frame is the suspected cause of the
                // "Done" freeze, and this path does the same pair of
                // operations. The mitigation is untested.
                let scheduler_handle_cell = scheduler_handle_cell.clone();
                let refresh_list = refresh_list.clone();
                glib::idle_add_local_once(move || {
                    request_reschedule(&scheduler_handle_cell);
                    refresh_list();
                });
            }
        }
    });

    done_button.connect_clicked({
        let commit_and_close = commit_and_close.clone();
        move |_| commit_and_close()
    });

    dialog.present();
}

/// Whether an enabled alarm vibrates, the Silent override is off, and
/// feedbackd is in the Silent profile.
fn silent_banner_needed(alarms_path: &std::path::Path) -> bool {
    if cajole::settings::AppSettings::load(settings_path())
        .unwrap_or_default()
        .vibrate_in_silent
    {
        return false;
    }
    let any_vibrating = AlarmStore::load(alarms_path)
        .map(|store| store.alarms().iter().any(|a| a.enabled && a.vibration_enabled))
        .unwrap_or(false);
    if !any_vibrating {
        return false;
    }
    let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return false;
    };
    cajole::silent_override::current_profile(&conn).as_deref() == Some("silent")
}

/// The Settings screen. It currently holds only autostart, but is its own
/// screen so app-wide settings (the speaker sink hardcoded in `scheduler.rs`
/// is the likely next) have a place to go.
fn show_settings(parent: &adw::ApplicationWindow) {
    let window = adw::PreferencesWindow::builder()
        .transient_for(parent)
        .modal(true)
        .search_enabled(false)
        .title("Settings")
        .build();

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Startup")
        .build();

    let autostart_row = adw::SwitchRow::builder()
        .title("Run in Background at Login")
        .subtitle("Required for alarms to ring without opening Cajole first")
        .active(cajole::autostart::is_enabled())
        .build();
    // Set while the switch is reverted after a failure, so the revert does not
    // start another request.
    let reverting = Rc::new(Cell::new(false));
    autostart_row.connect_active_notify(move |row| {
        if reverting.get() {
            return;
        }
        let wanted = row.is_active();
        let row = row.clone();
        let reverting = reverting.clone();
        // The portal can wait on a permission dialog.
        row.set_sensitive(false);
        glib::spawn_future_local(async move {
            let result = if wanted {
                cajole::autostart::enable().await
            } else {
                cajole::autostart::disable().await
            };
            if let Err(err) = result {
                tracing::error!(%err, "failed to update autostart setting");
                reverting.set(true);
                row.set_active(!wanted);
                reverting.set(false);
            }
            row.set_sensitive(true);
        });
    });
    group.add(&autostart_row);
    page.add(&group);

    let settings_path = settings_path();
    let settings = Rc::new(RefCell::new(
        cajole::settings::AppSettings::load(&settings_path).unwrap_or_default(),
    ));

    // Same "(None)" pattern as the alarm sounds: an explicit choice instead of
    // an unset value.
    //
    // Each entry is a destination: a sink, or a specific port on a sink with
    // several audible routes (earpiece vs. speaker on one sink).
    let destinations = cajole::settings::discover_destinations();
    let mut destination_labels: Vec<String> = vec!["(None)".to_string()];
    let destinations_by_index: Rc<Vec<Option<cajole::settings::AudioDestination>>> = Rc::new({
        let mut items = vec![None];
        for d in &destinations {
            destination_labels.push(if d.is_internal {
                format!("{} (built-in)", d.description)
            } else {
                d.description.clone()
            });
            items.push(Some(d.destination.clone()));
        }
        items
    });
    let label_refs: Vec<&str> = destination_labels.iter().map(String::as_str).collect();

    // No "Speaker Destination" row: each alarm picks its own in its "Play on"
    // row. Only the backup, used when an alarm's chosen output is unavailable,
    // is global, since it is a standing preference about the device.
    let audio_group = adw::PreferencesGroup::builder().title("Audio").build();

    let backup_row = adw::ComboRow::builder()
        .title("Backup Sink")
        .subtitle(
            "Used when an alarm's configured sink is unavailable. A permanent, non-removable \
             device is recommended.",
        )
        .model(&gtk4::StringList::new(&label_refs))
        .build();
    backup_row.set_selected(
        destinations_by_index
            .iter()
            .position(|d| *d == settings.borrow().backup_sink)
            .unwrap_or(0) as u32,
    );
    backup_row.connect_selected_notify({
        let settings = settings.clone();
        let destinations_by_index = destinations_by_index.clone();
        let settings_path = settings_path.clone();
        move |row| {
            let mut s = settings.borrow_mut();
            s.backup_sink = destinations_by_index
                .get(row.selected() as usize)
                .cloned()
                .flatten();
            if let Err(err) = s.save(&settings_path) {
                tracing::error!(%err, "failed to save backup sink setting");
            }
        }
    });
    audio_group.add(&backup_row);

    page.add(&audio_group);

    // Hidden until feedbackd is confirmed present.
    let vibration_group = adw::PreferencesGroup::builder()
        .title("Vibration")
        .visible(false)
        .build();
    let silent_row = adw::SwitchRow::builder()
        .title("Vibrate Even in Silent Mode")
        .subtitle(
            "Switches the feedback profile from Silent to Quiet while an alarm vibrates or a \
             vibration is previewed, then restores it",
        )
        .active(settings.borrow().vibrate_in_silent)
        .build();
    silent_row.connect_active_notify({
        let settings = settings.clone();
        let settings_path = settings_path.clone();
        let parent = parent.clone();
        move |row| {
            let mut s = settings.borrow_mut();
            s.vibrate_in_silent = row.is_active();
            if let Err(err) = s.save(&settings_path) {
                tracing::error!(%err, "failed to save vibrate-in-silent setting");
            }
            if let Some(app) = parent.application() {
                app.activate_action("update-silent-banner", None);
            }
        }
    });
    vibration_group.add(&silent_row);
    page.add(&vibration_group);
    glib::spawn_future_local({
        let vibration_group = vibration_group.clone();
        async move {
            if let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await {
                vibration_group.set_visible(cajole::feedback::vibration_available(&conn).await);
            }
        }
    });
    window.add(&page);

    window.present();
}

/// The standard GNOME About window, built from `AdwAboutWindow` for the
/// platform layout and its "Troubleshooting" section.
fn show_about_dialog(parent: &adw::ApplicationWindow) {
    let about = adw::AboutWindow::builder()
        .transient_for(parent)
        .modal(true)
        .application_name("Cajole")
        .application_icon("net.dunevent.Cajole")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Cajole contributors")
        .copyright("\u{a9} 2026 Cajole contributors")
        .license_type(gtk4::License::Gpl30)
        .comments("A gentle, mobile-first alarm clock for GTK4/libadwaita Linux phones.")
        .website("https://dev.dunevent.net/aj/Cajole")
        .issue_url("https://dev.dunevent.net/aj/Cajole/issues")
        .build();
    about.add_link("Donate", "https://paypal.me/ajdunevent");
    about.add_link("Fediverse", "https://social.dunevent.net/@aj");

    // Each of these is an on-demand Sound Manager source. None is bundled or
    // distributed with this package, and nothing is pre-installed; a person
    // must choose to download a sound. GNOME Clocks is credited separately
    // because its scheduling and sound-playback approach informed decisions
    // throughout this project. This is not Legal-section material, which
    // covers only what the package ships, and it ships no sounds.
    about.add_acknowledgement_section(
        Some("With thanks to"),
        &[
            "GNOME Clocks — design and architecture reference, and an optional downloadable sound source",
            "AOSP (Android Open Source Project) — an optional downloadable sound source",
            "KDE Plasma (Ocean Sound Theme) — an optional downloadable sound source",
            "KDE Plasma Mobile — an optional downloadable sound source",
        ],
    );

    about.present();
}

/// The removal action, shared by the "nothing references this" path and the
/// "confirmed despite alarms using it" response, so there is one
/// implementation.
///
/// `is_default` selects the two-stage behavior: the first press on a
/// downloaded `Default` entry only clears its local file (demoting it to "not
/// downloaded", row still visible); every other case (a `Custom` entry, or a
/// second press on a cleared `Default`) removes or hides the row.
#[allow(clippy::too_many_arguments)]
fn perform_sound_removal(
    entry_id: Uuid,
    is_default: bool,
    list_box: &gtk4::ListBox,
    row: &adw::ActionRow,
    restore_button: &gtk4::Button,
    play_button: &gtk4::Button,
    download_button: &Option<gtk4::Button>,
    row_state: &Rc<Cell<RowPlayState>>,
    current_path: &Rc<RefCell<Option<String>>>,
    on_change: &Rc<dyn Fn()>,
) {
    let is_downloaded_default = is_default && current_path.borrow().is_some();
    if is_downloaded_default {
        if let Ok(mut library) = cajole::sound_library::SoundLibrary::load(sound_library_path()) {
            if let Err(err) = library.clear_download(entry_id) {
                tracing::warn!(%err, "failed to remove downloaded sound");
            }
        }
        *current_path.borrow_mut() = None;
        if let Some(button) = download_button {
            button.set_sensitive(true);
        }
        row_state.set(RowPlayState::Idle);
        play_button.set_icon_name("media-playback-start-symbolic");
        play_button.set_tooltip_text(Some("Preview"));
    } else {
        if let Ok(mut library) = cajole::sound_library::SoundLibrary::load(sound_library_path()) {
            if let Err(err) = library.remove(entry_id) {
                tracing::warn!(%err, "failed to remove sound");
            }
            restore_button.set_sensitive(library.has_removed_defaults());
        }
        list_box.remove(row);
    }
    // Notifies whoever opened this Sound Manager (typically the alarm form's
    // "Manage Sounds…" button) so its picker rebuilds. Without this it kept
    // showing a removed sound.
    on_change();
}

/// The Sound Manager: add, preview, download and remove alarm sounds.
/// `on_change` is called after anything that could change the available
/// sounds, so the alarm form's picker (if opened from one) can refresh its
/// model.
fn show_sound_manager(
    parent: &impl IsA<gtk4::Window>,
    alarm_store_path: PathBuf,
    toast_overlay: adw::ToastOverlay,
    on_change: impl Fn() + 'static,
) {
    let on_change: Rc<dyn Fn()> = Rc::new(on_change);

    // Lives on the main window's toast overlay, not the Sound Manager's
    // window. The row widgets showing progress are destroyed with that window,
    // but the download (an independent OS thread) continues, and a main-window
    // toast keeps it visible.
    //
    // A persistent "Downloading N sounds" toast (`timeout(0)`) tracks
    // downloads in flight. Each completion briefly replaces it with a
    // transient "{name} downloaded" toast, and the persistent one (count
    // decremented) returns when that toast's timeout elapses, so simultaneous
    // downloads are announced one at a time.
    let active_downloads: Rc<RefCell<std::collections::HashMap<Uuid, String>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));
    let persistent_download_toast: Rc<RefCell<Option<adw::Toast>>> = Rc::new(RefCell::new(None));

    fn sync_persistent_download_toast(
        toast_overlay: &adw::ToastOverlay,
        active_downloads: &Rc<RefCell<std::collections::HashMap<Uuid, String>>>,
        persistent_download_toast: &Rc<RefCell<Option<adw::Toast>>>,
    ) {
        let count = active_downloads.borrow().len();
        if count == 0 {
            if let Some(toast) = persistent_download_toast.borrow_mut().take() {
                toast.dismiss();
            }
            return;
        }
        let title = if count == 1 {
            "Downloading 1 sound".to_string()
        } else {
            format!("Downloading {count} sounds")
        };
        let mut current = persistent_download_toast.borrow_mut();
        if let Some(existing) = current.as_ref() {
            existing.set_title(&title);
        } else {
            let toast = adw::Toast::builder().title(title).timeout(0).build();
            toast_overlay.add_toast(toast.clone());
            *current = Some(toast);
        }
    }

    // Called when a download succeeds: replaces the "Downloading N sounds"
    // toast (if showing) with a transient "{name} downloaded" one, and
    // restores the persistent toast (count already decremented, since the
    // caller removes the download from `active_downloads` first) from that
    // toast's `connect_dismissed` callback. Sounds finishing close together
    // each get their turn.
    fn announce_download_complete(
        toast_overlay: &adw::ToastOverlay,
        label: &str,
        active_downloads: &Rc<RefCell<std::collections::HashMap<Uuid, String>>>,
        persistent_download_toast: &Rc<RefCell<Option<adw::Toast>>>,
    ) {
        if let Some(toast) = persistent_download_toast.borrow_mut().take() {
            toast.dismiss();
        }
        let done_toast = adw::Toast::builder().title(format!("{label} downloaded")).build();
        done_toast.connect_dismissed({
            let toast_overlay = toast_overlay.clone();
            let active_downloads = active_downloads.clone();
            let persistent_download_toast = persistent_download_toast.clone();
            move |_| {
                sync_persistent_download_toast(&toast_overlay, &active_downloads, &persistent_download_toast);
            }
        });
        toast_overlay.add_toast(done_toast);
    }

    // Only one preview plays at a time: starting one stops the previous.
    let currently_previewing: Rc<RefCell<Option<ActivePreview>>> = Rc::new(RefCell::new(None));

    let list_box = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();
    let clamp = adw::Clamp::builder().child(&list_box).build();
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&clamp)
        .vexpand(true)
        .build();

    let add_button = gtk4::Button::from_icon_name("list-add-symbolic");
    add_button.set_tooltip_text(Some("Add a custom sound"));
    let restore_button = gtk4::Button::builder().label("Restore Defaults").build();

    let header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new("Sounds", ""))
        .build();
    header.pack_start(&add_button);
    header.pack_end(&restore_button);

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    content.append(&scrolled);

    let toolbar_view = adw::ToolbarView::builder().content(&content).build();
    toolbar_view.add_top_bar(&header);

    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .default_height(680)
        .content(&toolbar_view)
        .build();

    // `refresh` does a full rebuild and re-sort. It runs when the dialog
    // opens, after adding a custom sound and after "Restore Defaults". Row
    // actions (download, play/stop, remove) update their own row in place, so
    // a row does not move while being downloaded or played.
    let refresh: Rc<dyn Fn()> = {
        let list_box = list_box.clone();
        let restore_button = restore_button.clone();
        let on_change = on_change.clone();
        let currently_previewing = currently_previewing.clone();
        // The Sound Manager's own window, not the outer `parent`. A dialog
        // presented against `parent` would be a sibling of this window, not a
        // child, and could render behind it. An owned `adw::Window` captured
        // by value needs no lifetime workarounds.
        let sound_manager_window = dialog.clone();
        Rc::new(move || {
            while let Some(row) = list_box.row_at_index(0) {
                list_box.remove(&row);
            }
            let Ok(library) = cajole::sound_library::SoundLibrary::load(sound_library_path())
            else {
                return;
            };

            // Nothing to restore if nothing was removed. Insensitive, not
            // hidden, so the header layout stays stable.
            restore_button.set_sensitive(library.has_removed_defaults());

            for entry in library.sorted_visible_entries() {
                let row = adw::ActionRow::builder()
                    .title(entry.label.clone())
                    .title_lines(1)
                    .subtitle_lines(1)
                    .build();
                match &entry.origin {
                    cajole::sound_library::SoundOrigin::Custom => {
                        row.set_subtitle("Source: User provided");
                    }
                    cajole::sound_library::SoundOrigin::Default { project, .. } => {
                        row.set_subtitle(&format!("Source: {project}"));
                    }
                }

                if let cajole::sound_library::SoundOrigin::Default {
                    project,
                    attribution,
                    license,
                    ..
                } = &entry.origin
                {
                    let popover_body = gtk4::Box::builder()
                        .orientation(gtk4::Orientation::Vertical)
                        .spacing(4)
                        .margin_top(10)
                        .margin_bottom(10)
                        .margin_start(12)
                        .margin_end(12)
                        .build();
                    let project_label = gtk4::Label::builder()
                        .label(project)
                        .halign(gtk4::Align::Start)
                        .css_classes(["heading"])
                        .build();
                    let license_label = gtk4::Label::builder()
                        .label(license)
                        .halign(gtk4::Align::Start)
                        .css_classes(["dim-label", "caption"])
                        .build();
                    let attribution_label = gtk4::Label::builder()
                        .label(attribution)
                        .halign(gtk4::Align::Start)
                        .wrap(true)
                        .max_width_chars(28)
                        .build();
                    popover_body.append(&project_label);
                    popover_body.append(&license_label);
                    popover_body.append(&attribution_label);
                    let popover = gtk4::Popover::builder().child(&popover_body).build();
                    let info_button = gtk4::MenuButton::builder()
                        .icon_name("dialog-information-symbolic")
                        .valign(gtk4::Align::Center)
                        .tooltip_text("Source and license")
                        .css_classes(["flat"])
                        .popover(&popover)
                        .build();
                    row.add_suffix(&info_button);
                }

                let is_default = matches!(
                    entry.origin,
                    cajole::sound_library::SoundOrigin::Default { .. }
                );
                let source_url = match &entry.origin {
                    cajole::sound_library::SoundOrigin::Default { source_url, .. } => {
                        Some(source_url.clone())
                    }
                    cajole::sound_library::SoundOrigin::Custom => None,
                };
                let entry_id = entry.id;
                let entry_label = entry.label.clone();
                let dest_path = downloaded_sounds_dir().join(format!("{entry_id}.audio"));
                // This row's playable file, independent of the snapshot
                // `entry.file_path` was built from. Updated when a download
                // completes (Play-triggered or the Download button) so a later
                // click sees the file is present and does not re-download it.
                let current_path: Rc<RefCell<Option<String>>> =
                    Rc::new(RefCell::new(entry.file_path.clone()));
                let row_state: Rc<Cell<RowPlayState>> = Rc::new(Cell::new(RowPlayState::Idle));
                // Set while a download triggered by this row is in flight, so
                // Stop during `Downloading` can kill the `curl` process.
                // Cleared when the download finishes, fails or is cancelled.
                let download_cancel: Rc<RefCell<Option<DownloadCancelHandle>>> =
                    Rc::new(RefCell::new(None));

                let play_button = gtk4::Button::builder()
                    .icon_name("media-playback-start-symbolic")
                    .valign(gtk4::Align::Center)
                    .tooltip_text("Preview")
                    .css_classes(["flat"])
                    .build();

                let download_button = if is_default {
                    let button = gtk4::Button::builder()
                        .icon_name("folder-download-symbolic")
                        .valign(gtk4::Align::Center)
                        .tooltip_text("Download")
                        .css_classes(["flat"])
                        .sensitive(entry.file_path.is_none())
                        .build();
                    Some(button)
                } else {
                    None
                };

                row.add_suffix(&play_button);
                if let Some(download_button) = &download_button {
                    row.add_suffix(download_button);
                }

                // Shared by the Play-triggered auto-play and the Download
                // button: records the path and disables the Download button.
                // The button stays visible; the app disables icons instead of
                // hiding them.
                fn mark_downloaded_in_place(
                    entry_id: Uuid,
                    path: &str,
                    current_path: &Rc<RefCell<Option<String>>>,
                    download_button: &Option<gtk4::Button>,
                ) {
                    if let Ok(mut library) =
                        cajole::sound_library::SoundLibrary::load(sound_library_path())
                    {
                        if let Err(err) = library.mark_downloaded(entry_id, path.to_string()) {
                            tracing::error!(%err, "failed to record downloaded sound");
                        }
                    }
                    *current_path.borrow_mut() = Some(path.to_string());
                    if let Some(button) = download_button {
                        button.set_sensitive(false);
                    }
                }

                /// Cancels this row's in-flight download and reverts its icons
                /// immediately (optimistic). The failure from the channel
                /// arrives later and repeats the revert, which is harmless.
                fn cancel_download_in_place(
                    download_cancel: &Rc<RefCell<Option<DownloadCancelHandle>>>,
                    row_state: &Rc<Cell<RowPlayState>>,
                    play_button: &gtk4::Button,
                    download_button: &Option<gtk4::Button>,
                ) {
                    if let Some(handle) = download_cancel.borrow_mut().take() {
                        cancel_download(&handle);
                    }
                    row_state.set(RowPlayState::Idle);
                    play_button.set_icon_name("media-playback-start-symbolic");
                    play_button.set_tooltip_text(Some("Preview"));
                    if let Some(button) = download_button {
                        button.set_sensitive(true);
                    }
                }

                fn start_playing_in_place(
                    path: String,
                    play_button: &gtk4::Button,
                    row_state: &Rc<Cell<RowPlayState>>,
                    currently_previewing: &Rc<RefCell<Option<ActivePreview>>>,
                ) {
                    stop_active_preview(currently_previewing);
                    match play_sound_preview(&path) {
                        Ok(subprocess) => {
                            row_state.set(RowPlayState::Playing);
                            play_button.set_icon_name("media-playback-stop-symbolic");
                            play_button.set_tooltip_text(Some("Stop"));

                            // Watches for the sound finishing on its own (not
                            // via Stop) so the icon reverts either way.
                            let watch_subprocess = subprocess.clone();
                            let row_state_watch = row_state.clone();
                            let play_button_watch = play_button.clone();
                            let currently_previewing_watch = currently_previewing.clone();
                            glib::MainContext::default().spawn_local(async move {
                                let _ = watch_subprocess.wait_check_future().await;
                                if row_state_watch.get() == RowPlayState::Playing {
                                    row_state_watch.set(RowPlayState::Idle);
                                    play_button_watch
                                        .set_icon_name("media-playback-start-symbolic");
                                    play_button_watch.set_tooltip_text(Some("Preview"));
                                    currently_previewing_watch.borrow_mut().take();
                                }
                            });

                            let row_state_stop = row_state.clone();
                            let play_button_stop = play_button.clone();
                            *currently_previewing.borrow_mut() = Some(ActivePreview {
                                subprocess,
                                on_stop: Box::new(move || {
                                    row_state_stop.set(RowPlayState::Idle);
                                    play_button_stop.set_icon_name("media-playback-start-symbolic");
                                    play_button_stop.set_tooltip_text(Some("Preview"));
                                }),
                            });
                        }
                        Err(err) => {
                            tracing::warn!(%err, "failed to preview sound");
                            row_state.set(RowPlayState::Idle);
                            play_button.set_icon_name("media-playback-start-symbolic");
                            play_button.set_tooltip_text(Some("Preview"));
                        }
                    }
                }

                play_button.connect_clicked({
                    let currently_previewing = currently_previewing.clone();
                    let row_state = row_state.clone();
                    let download_cancel = download_cancel.clone();
                    let play_button = play_button.clone();
                    let download_button = download_button.clone();
                    let source_url = source_url.clone();
                    let dest_path = dest_path.clone();
                    let current_path = current_path.clone();
                    let toast_overlay = toast_overlay.clone();
                    let active_downloads = active_downloads.clone();
                    let persistent_download_toast = persistent_download_toast.clone();
                    let entry_label = entry_label.clone();
                    move |_| match row_state.get() {
                        RowPlayState::Idle => {
                            let path = current_path.borrow().clone();
                            if let Some(path) = path {
                                start_playing_in_place(
                                    path,
                                    &play_button,
                                    &row_state,
                                    &currently_previewing,
                                );
                            } else if let Some(source_url) = source_url.clone() {
                                stop_active_preview(&currently_previewing);
                                row_state.set(RowPlayState::Downloading);
                                play_button.set_icon_name("media-playback-stop-symbolic");
                                play_button.set_tooltip_text(Some("Stop"));
                                if let Some(button) = &download_button {
                                    button.set_sensitive(false);
                                }

                                active_downloads.borrow_mut().insert(entry_id, entry_label.clone());
                                sync_persistent_download_toast(
                                    &toast_overlay,
                                    &active_downloads,
                                    &persistent_download_toast,
                                );

                                let (receiver, cancel_handle) =
                                    download_sound_async(source_url, dest_path.clone());
                                *download_cancel.borrow_mut() = Some(cancel_handle);
                                let row_state = row_state.clone();
                                let download_cancel = download_cancel.clone();
                                let play_button = play_button.clone();
                                let download_button = download_button.clone();
                                let currently_previewing = currently_previewing.clone();
                                let current_path = current_path.clone();
                                let dest_path = dest_path.clone();
                                let toast_overlay = toast_overlay.clone();
                                let active_downloads = active_downloads.clone();
                                let persistent_download_toast = persistent_download_toast.clone();
                                let entry_label = entry_label.clone();
                                glib::MainContext::default().spawn_local(async move {
                                    if let Ok(result) = receiver.recv().await {
                                        download_cancel.borrow_mut().take();
                                        active_downloads.borrow_mut().remove(&entry_id);
                                        match result {
                                            Ok(()) => {
                                                let path_str =
                                                    dest_path.to_string_lossy().to_string();
                                                mark_downloaded_in_place(
                                                    entry_id,
                                                    &path_str,
                                                    &current_path,
                                                    &download_button,
                                                );
                                                start_playing_in_place(
                                                    path_str,
                                                    &play_button,
                                                    &row_state,
                                                    &currently_previewing,
                                                );
                                                announce_download_complete(
                                                    &toast_overlay,
                                                    &entry_label,
                                                    &active_downloads,
                                                    &persistent_download_toast,
                                                );
                                            }
                                            Err(err) => {
                                                // Covers a real failure (network, timeout,
                                                // bad URL) and a Stop click: a killed curl
                                                // reports back here like a failure, with
                                                // no separate cancelled path.
                                                tracing::warn!(%err, "sound download failed or was cancelled");
                                                row_state.set(RowPlayState::Idle);
                                                play_button.set_icon_name(
                                                    "media-playback-start-symbolic",
                                                );
                                                play_button.set_tooltip_text(Some("Preview"));
                                                if let Some(button) = &download_button {
                                                    button.set_sensitive(true);
                                                }
                                                sync_persistent_download_toast(
                                                    &toast_overlay,
                                                    &active_downloads,
                                                    &persistent_download_toast,
                                                );
                                            }
                                        }
                                    }
                                });
                            }
                        }
                        RowPlayState::Downloading => {
                            cancel_download_in_place(
                                &download_cancel,
                                &row_state,
                                &play_button,
                                &download_button,
                            );
                        }
                        RowPlayState::Playing => {
                            stop_active_preview(&currently_previewing);
                        }
                    }
                });

                if let Some(download_button) = &download_button {
                    download_button.connect_clicked({
                        let source_url = source_url.clone().unwrap_or_default();
                        let dest_path = dest_path.clone();
                        let current_path = current_path.clone();
                        let download_button = download_button.clone();
                        let toast_overlay = toast_overlay.clone();
                        let active_downloads = active_downloads.clone();
                        let persistent_download_toast = persistent_download_toast.clone();
                        let entry_label = entry_label.clone();
                        move |button| {
                            button.set_sensitive(false);

                            active_downloads.borrow_mut().insert(entry_id, entry_label.clone());
                            sync_persistent_download_toast(
                                &toast_overlay,
                                &active_downloads,
                                &persistent_download_toast,
                            );

                            let (receiver, _cancel_handle) =
                                download_sound_async(source_url.clone(), dest_path.clone());
                            let dest_path = dest_path.clone();
                            let current_path = current_path.clone();
                            let download_button = download_button.clone();
                            let toast_overlay = toast_overlay.clone();
                            let active_downloads = active_downloads.clone();
                            let persistent_download_toast = persistent_download_toast.clone();
                            let entry_label = entry_label.clone();
                            glib::MainContext::default().spawn_local(async move {
                                if let Ok(result) = receiver.recv().await {
                                    active_downloads.borrow_mut().remove(&entry_id);
                                    match result {
                                        Ok(()) => {
                                            let path_str = dest_path.to_string_lossy().to_string();
                                            mark_downloaded_in_place(
                                                entry_id,
                                                &path_str,
                                                &current_path,
                                                &Some(download_button),
                                            );
                                            announce_download_complete(
                                                &toast_overlay,
                                                &entry_label,
                                                &active_downloads,
                                                &persistent_download_toast,
                                            );
                                        }
                                        Err(err) => {
                                            tracing::warn!(%err, "sound download failed");
                                            download_button.set_sensitive(true);
                                            sync_persistent_download_toast(
                                                &toast_overlay,
                                                &active_downloads,
                                                &persistent_download_toast,
                                            );
                                        }
                                    }
                                }
                            });
                        }
                    });
                }

                let remove_button = gtk4::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .valign(gtk4::Align::Center)
                    .tooltip_text("Remove")
                    .css_classes(["flat"])
                    .build();
                row.add_suffix(&remove_button);
                remove_button.connect_clicked({
                    let is_default = is_default;
                    let list_box = list_box.clone();
                    let row = row.clone();
                    let restore_button = restore_button.clone();
                    let play_button = play_button.clone();
                    let download_button = download_button.clone();
                    let row_state = row_state.clone();
                    let current_path = current_path.clone();
                    let currently_previewing = currently_previewing.clone();
                    let window = sound_manager_window.clone();
                    let path = alarm_store_path.clone();
                    let on_change = on_change.clone();
                    move |_| {
                        if row_state.get() == RowPlayState::Playing {
                            stop_active_preview(&currently_previewing);
                        }

                        // Clearing a download (first press on a downloaded
                        // default) and removing an entry have the same effect
                        // to check: an alarm whose `sound_file` is this path
                        // would point at nothing. An entry that is not
                        // currently available has nothing to check.
                        let Some(file_path) = current_path.borrow().clone() else {
                            perform_sound_removal(
                                entry_id,
                                is_default,
                                &list_box,
                                &row,
                                &restore_button,
                                &play_button,
                                &download_button,
                                &row_state,
                                &current_path,
                                &on_change,
                            );
                            return;
                        };

                        // "(None)" is a selectable option in the alarm form,
                        // so at least one real sound need not be kept.
                        // Deleting the last one goes through the same in-use
                        // check as any other.
                        let in_use_labels: Vec<String> = AlarmStore::load(&path)
                            .map(|store| {
                                store
                                    .alarms()
                                    .iter()
                                    .filter(|a| a.sound_file.as_deref() == Some(file_path.as_str()))
                                    .map(|a| {
                                        if a.label.trim().is_empty() {
                                            "Untitled Alarm".to_string()
                                        } else {
                                            a.label.clone()
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                        if in_use_labels.is_empty() {
                            perform_sound_removal(
                                entry_id,
                                is_default,
                                &list_box,
                                &row,
                                &restore_button,
                                &play_button,
                                &download_button,
                                &row_state,
                                &current_path,
                                &on_change,
                            );
                            return;
                        }

                        let list_text = in_use_labels
                            .iter()
                            .map(|l| format!("\u{2022} {l}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let dialog = adw::AlertDialog::builder()
                            .heading("Sound In Use")
                            .body(format!(
                                "This sound is used by:\n\n{list_text}\n\nRemoving it will \
                                 reset those alarms to \"(None)\" — they'll still ring, just \
                                 without sound, until a new one is chosen for them."
                            ))
                            .build();
                        dialog.add_response("cancel", "Cancel");
                        dialog.add_response("remove", "Remove Anyway");
                        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                        dialog.set_default_response(Some("cancel"));
                        dialog.set_close_response("cancel");
                        dialog.connect_response(None, {
                            let list_box = list_box.clone();
                            let row = row.clone();
                            let restore_button = restore_button.clone();
                            let play_button = play_button.clone();
                            let download_button = download_button.clone();
                            let row_state = row_state.clone();
                            let current_path = current_path.clone();
                            let path = path.clone();
                            let file_path = file_path.clone();
                            let on_change = on_change.clone();
                            move |_, response| {
                                if response != "remove" {
                                    return;
                                }
                                if let Ok(mut store) = AlarmStore::load(&path) {
                                    let affected: Vec<Uuid> = store
                                        .alarms()
                                        .iter()
                                        .filter(|a| a.sound_file.as_deref() == Some(file_path.as_str()))
                                        .map(|a| a.id)
                                        .collect();
                                    for id in affected {
                                        if let Some(alarm) = store.get(id) {
                                            let mut updated = alarm.clone();
                                            updated.sound_file = None;
                                            if let Err(err) = store.upsert(updated) {
                                                tracing::warn!(%err, "failed to clear sound_file on affected alarm");
                                            }
                                        }
                                    }
                                }
                                perform_sound_removal(
                                    entry_id,
                                    is_default,
                                    &list_box,
                                    &row,
                                    &restore_button,
                                    &play_button,
                                    &download_button,
                                    &row_state,
                                    &current_path,
                                    &on_change,
                                );
                            }
                        });
                        dialog.present(Some(&window));
                    }
                });


                list_box.append(&row);
            }
            // Tells whoever opened this (e.g. the alarm form) that the
            // available sounds may have changed, so its picker stays in sync
            // without waiting for this dialog to close.
            on_change();
        })
    };
    refresh();

    add_button.connect_clicked({
        let dialog = dialog.clone();
        let refresh = refresh.clone();
        move |_| {
            add_custom_sound(&dialog, refresh.clone());
        }
    });

    restore_button.connect_clicked({
        let refresh = refresh.clone();
        move |_| {
            if let Ok(mut library) =
                cajole::sound_library::SoundLibrary::load(sound_library_path())
            {
                if let Err(err) = library.reset_defaults() {
                    tracing::warn!(%err, "failed to restore default sounds");
                }
            }
            refresh();
        }
    });

    dialog.present();
}

/// Prompts for a label, lets the person pick a file, copies it into the app's
/// sounds directory and registers it in the library.
fn add_custom_sound(parent: &adw::Window, refresh: Rc<dyn Fn()>) {
    let file_dialog = gtk4::FileDialog::builder()
        .title("Choose a sound file")
        .build();
    let parent = parent.clone();
    glib::MainContext::default().spawn_local(async move {
        let Ok(file) = file_dialog.open_future(Some(&parent)).await else {
            return;
        };
        let Some(source_path) = file.path() else {
            return;
        };

        let label_entry = adw::EntryRow::builder().title("Label").build();
        if let Some(guess) = source_path.file_stem().and_then(|s| s.to_str()) {
            label_entry.set_text(guess);
        }
        let group = adw::PreferencesGroup::builder().build();
        group.add(&label_entry);

        let cancel_button = gtk4::Button::with_label("Cancel");
        let add_button = gtk4::Button::builder()
            .label("Add")
            .css_classes(["suggested-action"])
            .build();
        let header = adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new("Name This Sound", ""))
            .build();
        header.pack_start(&cancel_button);
        header.pack_end(&add_button);

        let body = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(12)
            .margin_end(12)
            .build();
        body.append(&group);
        let content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .build();
        content.append(&body);
        let toolbar_view = adw::ToolbarView::builder().content(&content).build();
        toolbar_view.add_top_bar(&header);

        let label_dialog = adw::Window::builder()
            .transient_for(&parent)
            .modal(true)
            .default_width(360)
            .content(&toolbar_view)
            .build();

        cancel_button.connect_clicked({
            let label_dialog = label_dialog.clone();
            move |_| label_dialog.close()
        });
        add_button.connect_clicked({
            let label_dialog = label_dialog.clone();
            let label_entry = label_entry.clone();
            let refresh = refresh.clone();
            move |_| {
                let label = label_entry.text().to_string();
                let label = if label.trim().is_empty() {
                    "Custom Sound".to_string()
                } else {
                    label
                };

                let extension = source_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("audio");
                let dest_path =
                    custom_sounds_dir().join(format!("{}.{extension}", Uuid::new_v4()));
                if let Some(parent_dir) = dest_path.parent() {
                    let _ = std::fs::create_dir_all(parent_dir);
                }
                match std::fs::copy(&source_path, &dest_path) {
                    Ok(_) => {
                        if let Ok(mut library) =
                            cajole::sound_library::SoundLibrary::load(sound_library_path())
                        {
                            if let Err(err) = library.add_custom(
                                label,
                                dest_path.to_string_lossy().to_string(),
                            ) {
                                tracing::error!(%err, "failed to save custom sound");
                            }
                        }
                        refresh();
                    }
                    Err(err) => {
                        tracing::error!(%err, "failed to copy custom sound into place");
                    }
                }
                label_dialog.close();
            }
        });

        label_dialog.present();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekday_names_map_and_default_is_no_override() {
        assert_eq!(weekday_from_name("Monday"), Some(Weekday::Mon));
        assert_eq!(weekday_from_name("sunday"), Some(Weekday::Sun));
        assert_eq!(weekday_from_name("default"), None);
        assert_eq!(weekday_from_name(""), None);
    }

    #[test]
    fn display_hour_passes_through_unchanged_in_24h_mode() {
        assert_eq!(display_hour(0, false), 0);
        assert_eq!(display_hour(13, false), 13);
        assert_eq!(display_hour(23, false), 23);
    }

    #[test]
    fn display_hour_converts_midnight_and_noon_to_twelve_in_12h_mode() {
        assert_eq!(display_hour(0, true), 12); // 00:xx -> 12 AM
        assert_eq!(display_hour(12, true), 12); // 12:xx -> 12 PM
    }

    #[test]
    fn display_hour_converts_ordinary_hours_in_12h_mode() {
        assert_eq!(display_hour(1, true), 1);
        assert_eq!(display_hour(13, true), 1);
        assert_eq!(display_hour(23, true), 11);
        assert_eq!(display_hour(11, true), 11);
    }

    #[test]
    fn hour_24_from_form_passes_through_unchanged_in_24h_mode() {
        assert_eq!(hour_24_from_form(0, false, false), 0);
        assert_eq!(hour_24_from_form(23, true, false), 23); // is_pm ignored in 24h mode
    }

    #[test]
    fn hour_24_from_form_handles_twelve_am_and_twelve_pm_correctly() {
        // 12 AM is midnight (hour 0), not 12; 12 PM is noon (hour 12), not 0
        // or 24.
        assert_eq!(hour_24_from_form(12, false, true), 0);
        assert_eq!(hour_24_from_form(12, true, true), 12);
    }

    #[test]
    fn hour_24_from_form_handles_ordinary_hours() {
        assert_eq!(hour_24_from_form(1, false, true), 1);
        assert_eq!(hour_24_from_form(1, true, true), 13);
        assert_eq!(hour_24_from_form(11, false, true), 11);
        assert_eq!(hour_24_from_form(11, true, true), 23);
    }

    #[test]
    fn display_hour_and_hour_24_from_form_round_trip_for_every_hour() {
        // Round trip: for every 24h hour, converting to a 12h display value
        // and back reproduces the original.
        for hour in 0..24u32 {
            let is_pm = hour >= 12;
            let displayed = display_hour(hour, true);
            let recovered = hour_24_from_form(displayed, is_pm, true);
            assert_eq!(recovered, hour, "round trip failed for hour {hour}");
        }
    }

    #[test]
    fn week_order_from_monday_is_the_standard_order() {
        assert_eq!(
            week_order_from(Weekday::Mon),
            [
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
                Weekday::Sat,
                Weekday::Sun,
            ]
        );
    }

    #[test]
    fn week_order_from_sunday_wraps_correctly() {
        assert_eq!(
            week_order_from(Weekday::Sun),
            [
                Weekday::Sun,
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
                Weekday::Sat,
            ]
        );
    }

    #[test]
    fn week_order_from_saturday_wraps_correctly() {
        assert_eq!(
            week_order_from(Weekday::Sat),
            [
                Weekday::Sat,
                Weekday::Sun,
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ]
        );
    }

    #[test]
    fn repeat_summary_orders_days_by_the_given_first_weekday() {
        let mut repeat = RepeatDays::NONE;
        repeat.insert(Weekday::Wed);
        repeat.insert(Weekday::Fri);
        repeat.insert(Weekday::Sun);
        // Verifies the rotation `repeat_summary` uses puts Sunday first when
        // the locale does.
        let order = week_order_from(Weekday::Sun);
        let expected: Vec<_> = order
            .into_iter()
            .filter(|d| repeat.contains(*d))
            .map(day_abbreviation)
            .collect();
        assert_eq!(expected, vec!["Sun", "Wed", "Fri"]);
    }
}
