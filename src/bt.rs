//! Paired Bluetooth audio devices: state, name, battery, connect/disconnect.
//!
//! Connect/disconnect sends one-shot KS properties to the Bluetooth audio
//! driver, the same way Windows does (mechanism from ToothTray, BSD-2:
//! https://github.com/m2jean/ToothTray).

use std::collections::{BTreeMap, HashMap};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_ALLCLASSES, DIGCF_PRESENT, HDEVINFO, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDevicePropertyW,
};
use windows::Win32::Devices::FunctionDiscovery::{PKEY_Device_ContainerId, PKEY_Device_FriendlyName};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_ContainerId, DEVPKEY_Device_FriendlyName, DEVPROPTYPE};
use windows::Win32::Foundation::{DEVPROPKEY, PROPERTYKEY};
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
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ};
use windows::core::{GUID, HSTRING, Interface, Result, w};

// Device ids of Bluetooth audio KS filters: bthenum (A2DP) or bthhfenum (Hands-Free).
const BT_AUDIO_DEVICE_ID_PREFIX: &str = r"{2}.\\?\bth";

// Battery percent (one byte) on the Hands-Free AG service node: the value
// Settings > Bluetooth shows.
const DEVPKEY_BLUETOOTH_BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5),
    pid: 2,
};

pub struct Device {
    /// Container id: stable across reconnects, shared by all nodes of the device.
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub battery: Option<u8>,
    controls: Vec<IKsControl>,
}

impl Device {
    pub fn connect(&self) -> Result<()> {
        self.send(KSPROPERTY_ONESHOT_RECONNECT)
    }

    pub fn disconnect(&self) -> Result<()> {
        self.send(KSPROPERTY_ONESHOT_DISCONNECT)
    }

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

/// Container ids come braced from audio endpoints and as raw GUIDs from SetupAPI.
fn container_key(container: &str) -> String {
    container.trim_matches(['{', '}']).to_uppercase()
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

#[derive(Default)]
struct BtNode {
    name: Option<String>,
    battery: Option<u8>,
}

unsafe fn device_property(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; 512];
    let mut property_type = DEVPROPTYPE::default();
    let mut size = 0;
    unsafe {
        SetupDiGetDevicePropertyW(set, data, key, &mut property_type, Some(&mut buffer), Some(&mut size), 0).ok()?;
    }
    buffer.truncate(size as usize);
    Some(buffer)
}

fn utf16_bytes_to_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

/// Device name and battery per container, read from the Bluetooth (BTHENUM) device nodes.
fn bt_nodes() -> HashMap<String, BtNode> {
    let mut nodes: HashMap<String, BtNode> = HashMap::new();
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(None, w!("BTHENUM"), None, DIGCF_ALLCLASSES | DIGCF_PRESENT) else {
            return nodes;
        };
        let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        let mut index = 0;
        while SetupDiEnumDeviceInfo(set, index, &mut data).is_ok() {
            index += 1;
            let Some(container) = device_property(set, &data, &DEVPKEY_Device_ContainerId) else {
                continue;
            };
            if container.len() != size_of::<GUID>() {
                continue;
            }
            let container = std::ptr::read_unaligned(container.as_ptr().cast::<GUID>());
            let node = nodes.entry(container_key(&format!("{container:?}"))).or_default();

            // The device itself is BTHENUM\DEV_<address>; the rest are its service nodes.
            let mut instance_id = [0u16; 256];
            if SetupDiGetDeviceInstanceIdW(set, &data, Some(&mut instance_id), None).is_ok()
                && String::from_utf16_lossy(&instance_id).contains(r"\DEV_")
            {
                node.name = device_property(set, &data, &DEVPKEY_Device_FriendlyName).map(|b| utf16_bytes_to_string(&b));
            }
            if let Some(battery) = device_property(set, &data, &DEVPKEY_BLUETOOTH_BATTERY) {
                node.battery = battery.first().copied();
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    nodes
}

/// All paired Bluetooth audio devices, one entry per physical device.
pub fn enumerate() -> Result<Vec<Device>> {
    let mut by_container: BTreeMap<String, Device> = BTreeMap::new();
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
            let id = container_key(&read_string(&endpoint, &PKEY_Device_ContainerId)?);
            let endpoint_name = read_string(&endpoint, &PKEY_Device_FriendlyName)?;
            let device = by_container.entry(id.clone()).or_insert_with(|| Device {
                id,
                name: endpoint_name,
                connected: false,
                battery: None,
                controls: Vec::new(),
            });
            device.connected |= endpoint.GetState()? == DEVICE_STATE_ACTIVE;
            device.controls.extend(controls);
        }
    }

    let mut nodes = bt_nodes();
    let mut devices: Vec<Device> = by_container.into_values().collect();
    for device in &mut devices {
        if let Some(node) = nodes.remove(&device.id) {
            device.name = node.name.unwrap_or(std::mem::take(&mut device.name));
            // The stored value is stale while the device is away.
            device.battery = node.battery.filter(|_| device.connected);
        }
    }
    Ok(devices)
}
