//! Android Head Tracker HID protocol over Linux hidraw.
//!
//! A head tracker is a HID sensor collection (usage page 0x20, usage 0xE1)
//! whose sensor description feature reads `#AndroidHeadTracker#<version>`.
//! The report layout is taken from the report descriptor instead of being
//! hard-coded, so trackers from any vendor work as long as they follow the
//! protocol.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const SENSOR_PAGE: u32 = 0x0020;
const fn sensor(id: u32) -> u32 {
    (SENSOR_PAGE << 16) | id
}

/// Application collection of the head tracker: "Other: Custom".
const USAGE_CUSTOM_SENSOR: u32 = sensor(0x00E1);
const USAGE_DESCRIPTION: u32 = sensor(0x0308);
const USAGE_REPORT_INTERVAL: u32 = sensor(0x030E);
const USAGE_REPORTING_STATE: u32 = sensor(0x0316);
const USAGE_REPORTING_ALL_EVENTS: u32 = sensor(0x0841);
const USAGE_POWER_STATE: u32 = sensor(0x0319);
const USAGE_POWER_FULL: u32 = sensor(0x0851);
/// Custom value 1: rotation vector (3 x int16, full scale = pi rad).
const USAGE_ROTATION: u32 = sensor(0x0544);
/// Custom value 2: angular velocity (3 x int16, full scale = 32 rad/s).
const USAGE_ANGULAR_VELOCITY: u32 = sensor(0x0545);
/// Custom value 3: reference frame discontinuity counter.
const USAGE_DISCONTINUITY: u32 = sensor(0x0546);

pub const DESCRIPTION_PREFIX: &str = "#AndroidHeadTracker#";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Input,
    Feature,
}

#[derive(Clone, Debug)]
struct Field {
    kind: Kind,
    report_id: u8,
    /// Bit offset inside the report, excluding the report ID byte.
    bit_offset: usize,
    size: u32,
    count: u32,
    logical_min: i32,
    logical_max: i32,
    /// Usage that owns the field (the logical collection usage for selectors).
    usage: u32,
    /// Selector usages for array/enum fields, in logical order.
    selectors: Vec<u32>,
}

impl Field {
    fn signed(&self) -> bool {
        self.logical_min < 0
    }

    fn read(&self, report: &[u8], index: u32) -> Option<i32> {
        let bit = self.bit_offset + (index * self.size) as usize;
        let raw = get_bits(report, bit, self.size as usize)?;
        Some(if self.signed() && self.size < 32 {
            let shift = 32 - self.size;
            ((raw << shift) as i32) >> shift
        } else {
            raw as i32
        })
    }

    fn write(&self, report: &mut [u8], value: i32) {
        set_bits(report, self.bit_offset, self.size as usize, value as u32);
    }

    fn selector_value(&self, usage: u32) -> Option<i32> {
        let index = self.selectors.iter().position(|&u| u == usage)?;
        Some(self.logical_min + index as i32)
    }
}

fn get_bits(report: &[u8], bit: usize, size: usize) -> Option<u32> {
    let mut value = 0u32;
    for i in 0..size {
        let b = bit + i;
        let byte = *report.get(b / 8)?;
        if byte >> (b % 8) & 1 != 0 {
            value |= 1 << i;
        }
    }
    Some(value)
}

fn set_bits(report: &mut [u8], bit: usize, size: usize, value: u32) {
    for i in 0..size {
        let b = bit + i;
        let mask = 1u8 << (b % 8);
        if value >> i & 1 != 0 {
            report[b / 8] |= mask;
        } else {
            report[b / 8] &= !mask;
        }
    }
}

/// Fields of a head tracker, located in its report descriptor.
#[derive(Clone, Debug)]
pub struct Layout {
    reporting_state: Field,
    power_state: Field,
    report_interval: Field,
    description: Option<Field>,
    rotation: Field,
    angular_velocity: Option<Field>,
    discontinuity: Option<Field>,
    numbered_reports: bool,
    feature_len: usize,
    description_report_len: usize,
}

#[derive(Clone, Copy, Debug)]
struct Globals {
    usage_page: u32,
    logical_min: i32,
    logical_max: i32,
    report_size: u32,
    report_count: u32,
    report_id: u8,
}

impl Layout {
    /// Parse a HID report descriptor and return the head tracker layout, if
    /// the descriptor contains one.
    pub fn parse(descriptor: &[u8]) -> Option<Layout> {
        let fields = parse_fields(descriptor)?;
        let numbered = fields.iter().any(|f| f.report_id != 0);
        let find = |kind: Kind, usage: u32| {
            fields
                .iter()
                .find(|f| f.kind == kind && f.usage == usage)
                .cloned()
        };

        let rotation = find(Kind::Input, USAGE_ROTATION)?;
        if rotation.count != 3 {
            return None;
        }
        let reporting_state = find(Kind::Feature, USAGE_REPORTING_STATE)?;
        let power_state = find(Kind::Feature, USAGE_POWER_STATE)?;
        let report_interval = find(Kind::Feature, USAGE_REPORT_INTERVAL)?;
        let description = find(Kind::Feature, USAGE_DESCRIPTION);

        let report_len = |kind: Kind, id: u8| {
            let bits = fields
                .iter()
                .filter(|f| f.kind == kind && f.report_id == id)
                .map(|f| f.bit_offset + (f.size * f.count) as usize)
                .max()
                .unwrap_or(0);
            bits.div_ceil(8) + usize::from(numbered)
        };
        let feature_len = report_len(Kind::Feature, reporting_state.report_id);
        let description_report_len = description
            .as_ref()
            .map(|d| report_len(Kind::Feature, d.report_id))
            .unwrap_or(0);

        Some(Layout {
            angular_velocity: find(Kind::Input, USAGE_ANGULAR_VELOCITY).filter(|f| f.count == 3),
            discontinuity: find(Kind::Input, USAGE_DISCONTINUITY),
            reporting_state,
            power_state,
            report_interval,
            description,
            rotation,
            numbered_reports: numbered,
            feature_len,
            description_report_len,
        })
    }
}

/// Walk the descriptor and collect every input/feature field of the
/// head-tracker application collection.
fn parse_fields(desc: &[u8]) -> Option<Vec<Field>> {
    let mut globals = Globals {
        usage_page: 0,
        logical_min: 0,
        logical_max: 0,
        report_size: 0,
        report_count: 0,
        report_id: 0,
    };
    let mut stack: Vec<Globals> = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    // (collection type, owning usage)
    let mut collections: Vec<(u8, u32)> = Vec::new();
    let mut in_tracker = false;
    let mut found_tracker = false;
    let mut offsets: Vec<(Kind, u8, usize)> = Vec::new();
    let mut fields = Vec::new();

    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            // Long item: skip.
            let len = *desc.get(i + 1)? as usize;
            i += 3 + len;
            continue;
        }
        let size = match prefix & 0x3 {
            3 => 4,
            n => n as usize,
        };
        let data = desc.get(i + 1..i + 1 + size)?;
        i += 1 + size;
        let mut unsigned = 0u32;
        for (k, b) in data.iter().enumerate() {
            unsigned |= (*b as u32) << (8 * k);
        }
        let signed = match size {
            1 => unsigned as u8 as i8 as i32,
            2 => unsigned as u16 as i16 as i32,
            _ => unsigned as i32,
        };
        let full_usage = |page: u32| {
            if size == 4 {
                unsigned
            } else {
                (page << 16) | unsigned
            }
        };

        let item_type = (prefix >> 2) & 0x3;
        let tag = prefix >> 4;
        match (item_type, tag) {
            // Main items
            (0, 0x8) | (0, 0xB) => {
                let kind = if tag == 0x8 {
                    Kind::Input
                } else {
                    Kind::Feature
                };
                let bits = (globals.report_size * globals.report_count) as usize;
                let slot = offsets
                    .iter_mut()
                    .position(|(k, id, _)| *k == kind && *id == globals.report_id);
                let bit_offset = match slot {
                    Some(p) => {
                        let off = offsets[p].2;
                        offsets[p].2 += bits;
                        off
                    }
                    None => {
                        offsets.push((kind, globals.report_id, bits));
                        0
                    }
                };
                let constant = unsigned & 1 != 0;
                if in_tracker && !constant || in_tracker && kind == Kind::Feature {
                    let (usage, selectors) = match collections.last() {
                        Some(&(2, owner)) => (owner, usages.clone()),
                        _ => (usages.first().copied().unwrap_or(0), Vec::new()),
                    };
                    let logical_max = if globals.logical_min >= 0 && globals.logical_max < 0 {
                        // Unsigned maximum encoded with the sign bit set.
                        i32::MAX
                    } else {
                        globals.logical_max
                    };
                    fields.push(Field {
                        kind,
                        report_id: globals.report_id,
                        bit_offset,
                        size: globals.report_size,
                        count: globals.report_count,
                        logical_min: globals.logical_min,
                        logical_max,
                        usage,
                        selectors,
                    });
                }
                usages.clear();
            }
            (0, 0x9) => usages.clear(), // Output
            (0, 0xA) => {
                let owner = usages.first().copied().unwrap_or(0);
                let ctype = data.first().copied().unwrap_or(0);
                if ctype == 1 && owner == USAGE_CUSTOM_SENSOR {
                    in_tracker = true;
                    found_tracker = true;
                }
                collections.push((ctype, owner));
                usages.clear();
            }
            (0, 0xC) => {
                if let Some((1, owner)) = collections.pop()
                    && owner == USAGE_CUSTOM_SENSOR
                {
                    in_tracker = false;
                }
                usages.clear();
            }
            // Global items
            (1, 0x0) => globals.usage_page = unsigned,
            (1, 0x1) => globals.logical_min = signed,
            (1, 0x2) => globals.logical_max = signed,
            (1, 0x7) => globals.report_size = unsigned,
            (1, 0x8) => globals.report_id = unsigned as u8,
            (1, 0x9) => globals.report_count = unsigned,
            (1, 0xA) => stack.push(globals),
            (1, 0xB) => globals = stack.pop()?,
            // Local items
            (2, 0x0) => usages.push(full_usage(globals.usage_page)),
            (2, 0x1) | (2, 0x2) => {}
            _ => {}
        }
    }
    found_tracker.then_some(fields)
}

/// One orientation sample.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Rotation vector in radians (axis * angle), in the Android head frame:
    /// +X out of the right ear, +Y out of the nose, +Z out of the top of the head.
    pub rotation: [f32; 3],
    /// Angular velocity in rad/s, if the tracker reports it.
    pub angular_velocity: Option<[f32; 3]>,
    /// Reference frame discontinuity counter.
    pub discontinuity: Option<u32>,
}

/// A head tracker found in sysfs.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub devnode: PathBuf,
    pub name: String,
    /// Bluetooth address (HID_UNIQ), upper case with colons, if any.
    pub address: Option<String>,
    pub layout: Layout,
}

fn uevent_value(uevent: &str, key: &str) -> Option<String> {
    uevent.lines().find_map(|l| {
        l.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
            .map(str::to_owned)
    })
}

/// Inspect one hidraw sysfs directory (e.g. `/sys/class/hidraw/hidraw3`).
pub fn probe_sysfs(sys: &Path) -> Option<DeviceInfo> {
    let descriptor = fs::read(sys.join("device/report_descriptor")).ok()?;
    let layout = Layout::parse(&descriptor)?;
    let uevent = fs::read_to_string(sys.join("device/uevent")).unwrap_or_default();
    let name = uevent_value(&uevent, "HID_NAME").unwrap_or_else(|| "head tracker".into());
    let address = uevent_value(&uevent, "HID_UNIQ")
        .filter(|s| s.len() == 17)
        .map(|s| s.to_uppercase());
    let devnode = Path::new("/dev").join(sys.file_name()?);
    Some(DeviceInfo {
        devnode,
        name,
        address,
        layout,
    })
}

/// All head trackers currently present.
pub fn discover() -> Vec<DeviceInfo> {
    let Ok(dir) = fs::read_dir("/sys/class/hidraw") else {
        return Vec::new();
    };
    let mut found: Vec<DeviceInfo> = dir
        .flatten()
        .filter_map(|e| probe_sysfs(&e.path()))
        .collect();
    found.sort_by(|a, b| a.devnode.cmp(&b.devnode));
    found
}

// <linux/hidraw.h>
const fn hid_ioc(nr: u64, len: usize) -> u64 {
    // _IOC(_IOC_WRITE | _IOC_READ, 'H', nr, len)
    (3 << 30) | ((len as u64) << 16) | ((b'H' as u64) << 8) | nr
}
const HIDIOCSFEATURE: u64 = 0x06;
const HIDIOCGFEATURE: u64 = 0x07;

/// An opened head tracker.
pub struct Tracker {
    file: File,
    pub info: DeviceInfo,
    pub description: Option<String>,
}

impl Tracker {
    pub fn open(info: DeviceInfo) -> Result<Tracker> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&info.devnode)
            .with_context(|| {
                format!(
                    "cannot open {} (is the udev rule installed and are you logged in on this seat?)",
                    info.devnode.display()
                )
            })?;
        let mut tracker = Tracker {
            file,
            info,
            description: None,
        };
        tracker.description = tracker.read_description();
        if let Some(desc) = &tracker.description
            && !desc.starts_with(DESCRIPTION_PREFIX)
        {
            bail!(
                "{}: sensor description is {desc:?}, not an Android head tracker",
                tracker.info.devnode.display()
            );
        }
        Ok(tracker)
    }

    fn get_feature(&self, report_id: u8, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len.max(2)];
        buf[0] = report_id;
        let res = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hid_ioc(HIDIOCGFEATURE, buf.len()) as _,
                buf.as_mut_ptr(),
            )
        };
        if res < 0 {
            return Err(std::io::Error::last_os_error()).context("HIDIOCGFEATURE");
        }
        buf.truncate(res as usize);
        Ok(buf)
    }

    fn set_feature(&self, buf: &mut [u8]) -> Result<()> {
        let res = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hid_ioc(HIDIOCSFEATURE, buf.len()) as _,
                buf.as_mut_ptr(),
            )
        };
        if res < 0 {
            return Err(std::io::Error::last_os_error()).context("HIDIOCSFEATURE");
        }
        Ok(())
    }

    fn read_description(&self) -> Option<String> {
        let field = self.info.layout.description.as_ref()?;
        let report = self
            .get_feature(field.report_id, self.info.layout.description_report_len)
            .ok()?;
        let payload = &report[usize::from(self.info.layout.numbered_reports)..];
        let start = field.bit_offset / 8;
        let len = (field.size * field.count / 8) as usize;
        let bytes = payload.get(start..(start + len).min(payload.len()))?;
        let text: String = bytes
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        Some(text)
    }

    /// Turn the sensor on (or off) and set its report interval.
    pub fn enable(&self, on: bool, interval_ms: u32) -> Result<u32> {
        let layout = &self.info.layout;
        let id = layout.reporting_state.report_id;
        let mut report = self
            .get_feature(id, layout.feature_len)
            .unwrap_or_else(|_| vec![0u8; layout.feature_len]);
        report.resize(layout.feature_len, 0);
        report[0] = id;
        let offset = usize::from(layout.numbered_reports);
        let payload = &mut report[offset..];

        let reporting = if on {
            layout
                .reporting_state
                .selector_value(USAGE_REPORTING_ALL_EVENTS)
                .unwrap_or(layout.reporting_state.logical_min + 1)
        } else {
            layout.reporting_state.logical_min
        };
        let power = if on {
            layout
                .power_state
                .selector_value(USAGE_POWER_FULL)
                .unwrap_or(layout.power_state.logical_min + 1)
        } else {
            layout.power_state.logical_min
        };
        let interval = (interval_ms as i32).clamp(
            layout.report_interval.logical_min.max(1),
            layout.report_interval.logical_max,
        );
        layout.reporting_state.write(payload, reporting);
        layout.power_state.write(payload, power);
        layout.report_interval.write(payload, interval);
        self.set_feature(&mut report)?;
        Ok(interval as u32)
    }

    /// Wait up to `timeout_ms` for the next orientation sample.
    pub fn read(&mut self, timeout_ms: i32) -> Result<Option<Sample>> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let res = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if res < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(err.into());
        }
        if res == 0 {
            return Ok(None);
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            bail!("device disconnected");
        }
        let mut buf = [0u8; 256];
        let n = self.file.read(&mut buf)?;
        Ok(self.decode(&buf[..n]))
    }

    fn decode(&self, report: &[u8]) -> Option<Sample> {
        let layout = &self.info.layout;
        let payload = if layout.numbered_reports {
            if report.first() != Some(&layout.rotation.report_id) {
                return None;
            }
            &report[1..]
        } else {
            report
        };
        let rot = &layout.rotation;
        let scale = std::f32::consts::PI / rot.logical_max.max(1) as f32;
        let mut rotation = [0.0f32; 3];
        for (k, r) in rotation.iter_mut().enumerate() {
            *r = rot.read(payload, k as u32)? as f32 * scale;
        }
        let angular_velocity = layout.angular_velocity.as_ref().and_then(|f| {
            let scale = 32.0 / f.logical_max.max(1) as f32;
            Some([
                f.read(payload, 0)? as f32 * scale,
                f.read(payload, 1)? as f32 * scale,
                f.read(payload, 2)? as f32 * scale,
            ])
        });
        let discontinuity = layout
            .discontinuity
            .as_ref()
            .and_then(|f| f.read(payload, 0))
            .map(|v| v as u32);
        Some(Sample {
            rotation,
            angular_velocity,
            discontinuity,
        })
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        let _ = self.enable(false, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Report descriptor of a Sony WF-1000XM5 (firmware 2026).
    const WF1000XM5: &[u8] = &[
        0x05, 0x20, 0x09, 0xe1, 0xa1, 0x01, 0x85, 0x02, 0x0a, 0x08, 0x03, 0x15, 0x00, 0x25, 0x7f,
        0x75, 0x08, 0x95, 0x17, 0xb1, 0x03, 0x0a, 0x02, 0x03, 0x15, 0x80, 0x25, 0x7f, 0x75, 0x08,
        0x95, 0x10, 0xb1, 0x03, 0x85, 0x01, 0x0a, 0x16, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
        0x95, 0x01, 0xa1, 0x02, 0x0a, 0x40, 0x08, 0x0a, 0x41, 0x08, 0xb1, 0x00, 0xc0, 0x0a, 0x19,
        0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0xa1, 0x02, 0x0a, 0x55, 0x08, 0x0a,
        0x51, 0x08, 0xb1, 0x00, 0xc0, 0x0a, 0x0e, 0x03, 0x15, 0x00, 0x25, 0x3f, 0x35, 0x28, 0x45,
        0x29, 0x75, 0x06, 0x95, 0x01, 0x66, 0x01, 0x10, 0x55, 0x0d, 0xb1, 0x02, 0x0a, 0x44, 0x05,
        0x16, 0x01, 0x80, 0x26, 0xff, 0x7f, 0x37, 0x60, 0x4f, 0x46, 0xed, 0x47, 0xa1, 0xb0, 0xb9,
        0x12, 0x55, 0x08, 0x75, 0x10, 0x95, 0x03, 0x81, 0x02, 0x0a, 0x45, 0x05, 0x16, 0x01, 0x80,
        0x26, 0xff, 0x7f, 0x35, 0xe0, 0x45, 0x20, 0x55, 0x00, 0x75, 0x10, 0x95, 0x03, 0x81, 0x02,
        0x0a, 0x46, 0x05, 0x15, 0x00, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x55, 0x00, 0x75, 0x08,
        0x95, 0x01, 0x81, 0x02, 0xc0,
    ];

    #[test]
    fn parses_wf1000xm5() {
        let l = Layout::parse(WF1000XM5).expect("head tracker");
        assert!(l.numbered_reports);
        assert_eq!(l.rotation.report_id, 1);
        assert_eq!(l.rotation.bit_offset, 0);
        assert_eq!(l.angular_velocity.as_ref().unwrap().bit_offset, 48);
        assert_eq!(l.discontinuity.as_ref().unwrap().bit_offset, 96);
        assert_eq!(l.reporting_state.report_id, 1);
        assert_eq!(l.reporting_state.bit_offset, 0);
        assert_eq!(
            l.reporting_state.selector_value(USAGE_REPORTING_ALL_EVENTS),
            Some(1)
        );
        assert_eq!(l.power_state.bit_offset, 1);
        assert_eq!(l.power_state.selector_value(USAGE_POWER_FULL), Some(1));
        assert_eq!(l.report_interval.bit_offset, 2);
        assert_eq!(l.report_interval.logical_max, 63);
        assert_eq!(l.feature_len, 2);
        let d = l.description.as_ref().unwrap();
        assert_eq!((d.report_id, d.count), (2, 23));
        assert_eq!(l.description_report_len, 1 + 23 + 16);
    }

    #[test]
    fn rejects_other_devices() {
        // Generic boot keyboard descriptor start.
        let kbd = [0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0xc0];
        assert!(Layout::parse(&kbd).is_none());
    }

    #[test]
    fn signed_fields() {
        let l = Layout::parse(WF1000XM5).unwrap();
        let mut payload = [0u8; 13];
        // x = -32767, y = 16384, z = 0
        payload[0..2].copy_from_slice(&(-32767i16).to_le_bytes());
        payload[2..4].copy_from_slice(&16384i16.to_le_bytes());
        assert_eq!(l.rotation.read(&payload, 0), Some(-32767));
        assert_eq!(l.rotation.read(&payload, 1), Some(16384));
        assert_eq!(l.rotation.read(&payload, 2), Some(0));
    }
}
