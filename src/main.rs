//! Tray icon that connects/disconnects one Bluetooth audio device on left click.

#![windows_subsystem = "windows"]

mod bt;
mod sys;

use std::time::{Duration, Instant};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, SetTimer, TranslateMessage};

const TICK_MS: u32 = 250;
// ponytail: state is polled; switch to IMMNotificationClient if polling ever shows up in CPU.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

const BLUE: [u8; 3] = [0x60, 0xa5, 0xfa];
const AMBER: [u8; 3] = [0xfb, 0xbf, 0x24];
const GRAY: [u8; 3] = [0x9c, 0xa3, 0xaf];

#[derive(Clone, Copy, PartialEq)]
enum State {
    Disconnected,
    Connecting,
    Connected,
}

/// 32x32 headphones glyph: a headband arc over two ear cups.
fn headphones_icon(color: [u8; 3]) -> Icon {
    const SIZE: u32 = 32;
    const CENTER: f32 = 15.5;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32, y as f32);
            let distance = ((fx - CENTER).powi(2) + (fy - 17.0).powi(2)).sqrt();
            let band = fy <= 17.0 && (10.5..=13.5).contains(&distance);
            let cup = (16..=28).contains(&y) && ((2..=8).contains(&x) || (23..=29).contains(&x));
            let pixel = if band || cup { [color[0], color[1], color[2], 255] } else { [0; 4] };
            rgba.extend_from_slice(&pixel);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("icon buffer matches its size")
}

fn notify(text: &str) {
    // A failed toast has nowhere else to go: there is no console or window.
    let _ = sys::toast(text);
}

struct App {
    tray: TrayIcon,
    icons: [Icon; 3],
    devices: Vec<bt::Device>,
    selected: Option<String>,
    connecting_since: Option<Instant>,
    last_poll: Instant,
    shown: Option<(State, String)>,
    menu_devices: Vec<(String, String)>,
    device_items: Vec<(CheckMenuItem, String)>,
    autostart: CheckMenuItem,
    quit: MenuItem,
}

impl App {
    fn new() -> Self {
        let icons = [headphones_icon(GRAY), headphones_icon(AMBER), headphones_icon(BLUE)];
        let tray = TrayIconBuilder::new()
            .with_icon(icons[0].clone())
            .with_tooltip("bt-toggle")
            .with_menu_on_left_click(false)
            .build()
            .expect("tray icon is created");
        let mut app = Self {
            tray,
            icons,
            devices: Vec::new(),
            selected: sys::load_device(),
            connecting_since: None,
            last_poll: Instant::now(),
            shown: None,
            menu_devices: Vec::new(),
            device_items: Vec::new(),
            autostart: CheckMenuItem::new("Запускать при входе в Windows", true, sys::autostart_enabled(), None),
            quit: MenuItem::new("Выход", true, None),
        };
        app.poll();
        app
    }

    fn selected_device(&self) -> Option<&bt::Device> {
        let selected = self.selected.as_deref()?;
        self.devices.iter().find(|d| d.id == selected)
    }

    /// Returns false when the user asked to quit.
    fn handle_events(&mut self) -> bool {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                self.toggle();
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == *self.quit.id() {
                return false;
            }
            if event.id == *self.autostart.id() {
                let enabled = self.autostart.is_checked();
                if let Err(error) = sys::set_autostart(enabled) {
                    self.autostart.set_checked(!enabled);
                    notify(&format!("Не удалось изменить автозапуск: {error}"));
                }
            } else if let Some(id) = self.device_items.iter().find(|(item, _)| *item.id() == event.id).map(|(_, id)| id.clone()) {
                self.select(id);
            }
        }
        if self.last_poll.elapsed() >= POLL_INTERVAL {
            self.poll();
        }
        true
    }

    fn select(&mut self, id: String) {
        for (item, item_id) in &self.device_items {
            item.set_checked(*item_id == id);
        }
        if let Err(error) = sys::save_device(&id) {
            notify(&format!("Не удалось запомнить выбор: {error}"));
        }
        self.selected = Some(id);
        self.connecting_since = None;
        self.render();
    }

    fn toggle(&mut self) {
        self.poll();
        let Some(device) = self.selected_device() else {
            notify("Выбери наушники: правый клик по иконке");
            return;
        };
        let connected = device.connected;
        let result = if connected { device.disconnect() } else { device.connect() };
        match result {
            Ok(()) if !connected => self.connecting_since = Some(Instant::now()),
            Ok(()) => {}
            Err(error) => notify(&format!("{}: {error}", device.name)),
        }
        self.render();
    }

    fn poll(&mut self) {
        self.last_poll = Instant::now();
        self.devices = match bt::enumerate() {
            Ok(devices) => devices,
            Err(error) => {
                self.tray.set_tooltip(Some(format!("bt-toggle: {error}"))).ok();
                return;
            }
        };
        if let Some(since) = self.connecting_since {
            let device = self.selected_device();
            let connected = device.is_some_and(|d| d.connected);
            let name = device.map_or("Устройство".to_string(), |d| d.name.clone());
            if connected {
                self.connecting_since = None;
            } else if since.elapsed() > CONNECT_TIMEOUT {
                self.connecting_since = None;
                notify(&format!("{name} не подключились. Включены и не в кейсе?"));
            }
        }
        self.rebuild_menu_if_changed();
        self.render();
    }

    fn rebuild_menu_if_changed(&mut self) {
        let menu_devices: Vec<(String, String)> = self.devices.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
        if menu_devices == self.menu_devices {
            return;
        }
        let menu = Menu::new();
        self.device_items = menu_devices
            .iter()
            .map(|(id, name)| (CheckMenuItem::new(name, true, self.selected.as_ref() == Some(id), None), id.clone()))
            .collect();
        for (item, _) in &self.device_items {
            menu.append(item).ok();
        }
        if self.device_items.is_empty() {
            menu.append(&MenuItem::new("Нет сопряжённых аудиоустройств", false, None)).ok();
        }
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&self.autostart).ok();
        menu.append(&self.quit).ok();
        self.tray.set_menu(Some(Box::new(menu)));
        self.menu_devices = menu_devices;
    }

    fn render(&mut self) {
        let (state, tooltip) = match self.selected_device() {
            None if self.selected.is_none() => (State::Disconnected, "Выбери наушники: правый клик".to_string()),
            None => (State::Disconnected, "Выбранное устройство не найдено".to_string()),
            Some(device) if device.connected => {
                let battery = device.battery.map(|b| format!(" · {b}%")).unwrap_or_default();
                (State::Connected, format!("{}: подключены{battery}", device.name))
            }
            Some(device) if self.connecting_since.is_some() => (State::Connecting, format!("{}: подключаю…", device.name)),
            Some(device) => (State::Disconnected, format!("{}: отключены", device.name)),
        };
        if self.shown.as_ref().is_some_and(|shown| shown.0 == state && shown.1 == tooltip) {
            return;
        }
        self.tray.set_icon(Some(self.icons[state as usize].clone())).ok();
        self.tray.set_tooltip(Some(&tooltip)).ok();
        self.shown = Some((state, tooltip));
    }
}

fn main() {
    if sys::already_running() {
        return;
    }
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().expect("COM is initialized") };
    let _ = sys::register_toast_app();
    let mut app = App::new();

    // Thread timer: wakes the loop so tray/menu channels and polling are serviced.
    unsafe { SetTimer(None, 0, TICK_MS, None) };
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if !app.handle_events() {
            break;
        }
    }
}
