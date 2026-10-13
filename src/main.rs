//! Prototype: connect/disconnect a paired Bluetooth audio device the same way
//! Windows does, by sending one-shot KS properties to the Bluetooth audio
//! driver (mechanism from ToothTray, BSD-2: https://github.com/m2jean/ToothTray).

use std::collections::BTreeMap;

use windows::Win32::Devices::FunctionDiscovery::{PKEY_Device_ContainerId, PKEY_Device_FriendlyName};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATEMASK_ALL, IDeviceTopology, IMMDevice,
    IMMDeviceEnumerator, IPart, MMDeviceEnumerator, eAll,
};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, KSIDENTIFIER, KSIDENTIFIER_0, KSIDENTIFIER_0_0, KSPROPERTY_BTAUDIO,
    KSPROPERTY_ONESHOT_DISCONNECT, KSPROPERTY_ONESHOT_RECONNECT, KSPROPERTY_TYPE_GET,
    KSPROPSETID_BtAudio,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToBSTR;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ,
};
use windows::core::{HSTRING, Interface, Result};

// Device ids of Bluetooth audio KS filters: bthenum (A2DP) or bthhfenum (Hands-Free).
const BT_AUDIO_DEVICE_ID_PREFIX: &str = r"{2}.\\?\bth";

/// One physical device: all its audio endpoints share a container id.
struct BtAudioDevice {
    endpoint_names: Vec<String>,
    connected: bool,
    controls: Vec<IKsControl>,
}

impl BtAudioDevice {
    fn send(&self, property: KSPROPERTY_BTAUDIO) -> Result<()> {
        let identifier = KSIDENTIFIER {
            Anonymous: KSIDENTIFIER_0 {
                Anonymous: KSIDENTIFIER_0_0 {
                    Set: KSPROPSETID_BtAudio,
                    Id: property.0 as u32,
                    Flags: KSPROPERTY_TYPE_GET,
                },
            },
        };
        // Windows drops the link only after every endpoint (A2DP + HFP) is told to.
        for control in &self.controls {
            let mut bytes_returned = 0;
            unsafe {
                control.KsProperty(
                    &identifier,
                    size_of::<KSIDENTIFIER>() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut bytes_returned,
                )?;
            }
        }
        Ok(())
    }
}

fn read_string(device: &IMMDevice, key: &PROPERTYKEY) -> Result<String> {
    unsafe {
        let value = device.OpenPropertyStore(STGM_READ)?.GetValue(key)?;
        Ok(PropVariantToBSTR(&value)?.to_string())
    }
}

/// Finds the Bluetooth audio driver control behind each connector of the endpoint.
fn bt_controls(enumerator: &IMMDeviceEnumerator, endpoint: &IMMDevice) -> Result<Vec<IKsControl>> {
    let mut controls = Vec::new();
    unsafe {
        let topology: IDeviceTopology = endpoint.Activate(CLSCTX_ALL, None)?;
        for index in 0..topology.GetConnectorCount()? {
            let Ok(other) = topology.GetConnector(index)?.GetConnectedTo() else {
                continue;
            };
            let other_id = other.cast::<IPart>()?.GetTopologyObject()?.GetDeviceId()?.to_string()?;
            if !other_id.starts_with(BT_AUDIO_DEVICE_ID_PREFIX) {
                continue;
            }
            let driver = enumerator.GetDevice(&HSTRING::from(other_id))?;
            controls.push(driver.Activate(CLSCTX_ALL, None)?);
        }
    }
    Ok(controls)
}

fn enumerate() -> Result<Vec<BtAudioDevice>> {
    let mut by_container: BTreeMap<String, BtAudioDevice> = BTreeMap::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        // Capture endpoints too: the Hands-Free link may be reachable only via the mic,
        // and Windows keeps the device connected while HFP is up.
        let endpoints = enumerator.EnumAudioEndpoints(eAll, DEVICE_STATE(DEVICE_STATEMASK_ALL))?;
        for index in 0..endpoints.GetCount()? {
            let endpoint = endpoints.Item(index)?;
            // Stale endpoints of removed devices have no topology; skip them.
            let Ok(controls) = bt_controls(&enumerator, &endpoint) else {
                continue;
            };
            if controls.is_empty() {
                continue;
            }
            let device = by_container
                .entry(read_string(&endpoint, &PKEY_Device_ContainerId)?)
                .or_insert_with(|| BtAudioDevice {
                    endpoint_names: Vec::new(),
                    connected: false,
                    controls: Vec::new(),
                });
            device.endpoint_names.push(read_string(&endpoint, &PKEY_Device_FriendlyName)?);
            device.connected |= endpoint.GetState()? == DEVICE_STATE_ACTIVE;
            device.controls.extend(controls);
        }
    }
    Ok(by_container.into_values().collect())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, filter) = match args.as_slice() {
        [] => ("list", ""),
        [command] if command == "list" => ("list", ""),
        [command, filter] if command == "connect" || command == "disconnect" => {
            (command.as_str(), filter.as_str())
        }
        _ => {
            eprintln!("usage: bt-toggle [list | connect <name> | disconnect <name>]");
            std::process::exit(2);
        }
    };

    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let devices = enumerate()?;

    if command == "list" {
        for device in &devices {
            let state = if device.connected { "connected" } else { "disconnected" };
            println!("[{state}] {}", device.endpoint_names.join(" | "));
        }
        return Ok(());
    }

    let filter = filter.to_lowercase();
    let Some(device) = devices
        .iter()
        .find(|d| d.endpoint_names.iter().any(|n| n.to_lowercase().contains(&filter)))
    else {
        eprintln!("no Bluetooth audio device matches {filter:?}");
        std::process::exit(1);
    };
    let property = if command == "connect" { KSPROPERTY_ONESHOT_RECONNECT } else { KSPROPERTY_ONESHOT_DISCONNECT };
    device.send(property)?;
    println!("{command} sent to {}", device.endpoint_names.join(" | "));
    Ok(())
}
