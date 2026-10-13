//! Windows plumbing: settings and autostart in HKCU, toasts, single instance.

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::{HSTRING, PCWSTR, Result, w};

const APP_NAME: PCWSTR = w!("bt-toggle");
const SETTINGS_KEY: PCWSTR = w!(r"Software\bt-toggle");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
// Toasts from an unpackaged app need its AppUserModelID registered with a display name.
const AUMID: &str = "bt-toggle";
const AUMID_KEY: PCWSTR = w!(r"Software\Classes\AppUserModelId\bt-toggle");

fn read_string(subkey: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut size = size_of_val(&buffer) as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .to_hresult()
    .ok()
    .ok()?;
    let length = (size as usize / 2).saturating_sub(1); // size includes the terminating null
    Some(String::from_utf16_lossy(&buffer[..length]))
}

fn write_string(subkey: PCWSTR, value: PCWSTR, data: &str) -> Result<()> {
    let wide: Vec<u16> = data.encode_utf16().chain([0]).collect();
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            subkey,
            value,
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            size_of_val(wide.as_slice()) as u32,
        )
    }
    .to_hresult()
    .ok()
}

pub fn load_device() -> Option<String> {
    read_string(SETTINGS_KEY, w!("Device"))
}

pub fn save_device(id: &str) -> Result<()> {
    write_string(SETTINGS_KEY, w!("Device"), id)
}

pub fn autostart_enabled() -> bool {
    read_string(RUN_KEY, APP_NAME).is_some()
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    if !enabled {
        return unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, APP_NAME) }.to_hresult().ok();
    }
    let exe = std::env::current_exe()?;
    write_string(RUN_KEY, APP_NAME, &format!("\"{}\"", exe.display()))
}

pub fn register_toast_app() -> Result<()> {
    write_string(AUMID_KEY, w!("DisplayName"), "bt-toggle")
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn toast(text: &str) -> Result<()> {
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>{}</text></binding></visual></toast>"#,
        escape_xml(text)
    )))?;
    let notification = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&notification)
}

/// True if another copy already runs; the mutex lives as long as this process.
pub fn already_running() -> bool {
    unsafe { CreateMutexW(None, false, w!(r"Local\bt-toggle")).is_ok() && GetLastError() == ERROR_ALREADY_EXISTS }
}
