//! Ask BlueZ to open the HID channel of connected headphones.
//!
//! Headphones usually expose their head tracker as a Bluetooth HID service
//! but leave it to the host to connect it (Android does so for spatial
//! audio). BlueZ only connects audio profiles on its own, so the tracker
//! never shows up unless we call `ConnectProfile` for the HID UUID.

use std::collections::HashMap;

use anyhow::Result;
use zbus::blocking::Connection;
use zbus::blocking::fdo::ObjectManagerProxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub const HID_UUID: &str = "00001124-0000-1000-8000-00805f9b34fb";
const AUDIO_SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";

pub struct Headset {
    pub path: OwnedObjectPath,
    pub address: String,
    pub name: String,
}

pub struct Bluez {
    conn: Connection,
}

impl Bluez {
    pub fn new() -> Result<Self> {
        Ok(Bluez {
            conn: Connection::system()?,
        })
    }

    /// Connected devices that offer both an audio sink and a HID service.
    pub fn headsets(&self) -> Result<Vec<Headset>> {
        let om = ObjectManagerProxy::builder(&self.conn)
            .destination("org.bluez")?
            .path("/")?
            .build()?;
        let mut out = Vec::new();
        for (path, ifaces) in om.get_managed_objects()? {
            let Some(dev) = ifaces
                .iter()
                .find_map(|(name, props)| (name.as_str() == "org.bluez.Device1").then_some(props))
            else {
                continue;
            };
            if !prop_bool(dev, "Connected") {
                continue;
            }
            let uuids = prop_strings(dev, "UUIDs");
            let has = |u: &str| uuids.iter().any(|x| x.eq_ignore_ascii_case(u));
            if !has(HID_UUID) || !has(AUDIO_SINK_UUID) {
                continue;
            }
            out.push(Headset {
                path,
                address: prop_string(dev, "Address")
                    .unwrap_or_default()
                    .to_uppercase(),
                name: prop_string(dev, "Alias")
                    .or_else(|| prop_string(dev, "Name"))
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    pub fn connect_hid(&self, headset: &Headset) -> Result<()> {
        self.conn.call_method(
            Some("org.bluez"),
            headset.path.as_str(),
            Some("org.bluez.Device1"),
            "ConnectProfile",
            &(HID_UUID,),
        )?;
        Ok(())
    }
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

fn prop_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| String::try_from(v).ok())
}

fn prop_strings(props: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default()
}
