//! System tray icon (StatusNotifierItem): status, recentring, head tracking
//! and equaliser switches.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{MenuItem, ToolTip};

use crate::eq::EqMode;
use crate::tracking::Shared;

fn chinese() -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_some_and(|v| v.starts_with("zh"))
}

struct Text {
    title: &'static str,
    no_tracker: &'static str,
    front: &'static str,
    settling: &'static str,
    recenter: &'static str,
    tracking: &'static str,
    auto_recenter: &'static str,
    eq: &'static str,
    eq_front: &'static str,
    eq_diffuse: &'static str,
    eq_none: &'static str,
    quit: &'static str,
    hint: &'static str,
}

const ZH: Text = Text {
    title: "空间音频",
    no_tracker: "未找到头追耳机",
    front: "正前方声音",
    settling: "姿态收敛中…",
    recenter: "重新校准正前方",
    tracking: "头部追踪",
    auto_recenter: "静止 6 秒自动回中",
    eq: "均衡",
    eq_front: "前方音箱（音色接近原曲）",
    eq_diffuse: "扩散场",
    eq_none: "不均衡（原始 HRTF）",
    quit: "退出",
    hint: "左键：重新校准　中键：开关头追",
};

const EN: Text = Text {
    title: "Spatial Audio",
    no_tracker: "No head tracker",
    front: "Front source",
    settling: "Settling…",
    recenter: "Recenter",
    tracking: "Head tracking",
    auto_recenter: "Auto-recenter after 6 s still",
    eq: "Equaliser",
    eq_front: "Front speakers (original timbre)",
    eq_diffuse: "Diffuse field",
    eq_none: "None (raw HRTF)",
    quit: "Quit",
    hint: "Left click: recenter · Middle click: toggle tracking",
};

struct SpatialTray {
    shared: Arc<Shared>,
    text: &'static Text,
}

impl SpatialTray {
    fn status_line(&self) -> String {
        let st = self.shared.status.lock().unwrap().clone();
        match st.device {
            None => self.text.no_tracker.to_string(),
            Some(dev) if !st.centered => format!("{dev} · {}", self.text.settling),
            Some(dev) => format!(
                "{dev} · {} {:+.0}° / {:+.0}°",
                self.text.front, st.front_azimuth, st.front_elevation
            ),
        }
    }
}

impl ksni::Tray for SpatialTray {
    fn id(&self) -> String {
        "spatial-audio-linux".into()
    }

    fn title(&self) -> String {
        self.text.title.into()
    }

    fn icon_name(&self) -> String {
        "audio-headphones".into()
    }

    fn status(&self) -> ksni::Status {
        let has_tracker = self.shared.status.lock().unwrap().device.is_some();
        if has_tracker && self.shared.tracking.load(Ordering::Relaxed) {
            ksni::Status::Active
        } else {
            ksni::Status::Passive
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: "audio-headphones".into(),
            title: self.text.title.into(),
            description: format!("{}\n{}", self.status_line(), self.text.hint),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.shared.recenter.store(true, Ordering::Relaxed);
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.shared.tracking.fetch_xor(true, Ordering::Relaxed);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let t = self.text;
        let eq = EqMode::from_index(self.shared.eq.load(Ordering::Relaxed));
        vec![
            StandardItem {
                label: self.status_line(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: t.recenter.into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|this: &mut Self| {
                    this.shared.recenter.store(true, Ordering::Relaxed)
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: t.tracking.into(),
                checked: self.shared.tracking.load(Ordering::Relaxed),
                activate: Box::new(|this: &mut Self| {
                    this.shared.tracking.fetch_xor(true, Ordering::Relaxed);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: t.auto_recenter.into(),
                checked: self.shared.auto_recenter.load(Ordering::Relaxed),
                activate: Box::new(|this: &mut Self| {
                    this.shared.auto_recenter.fetch_xor(true, Ordering::Relaxed);
                }),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: t.eq.into(),
                submenu: vec![
                    RadioGroup {
                        selected: eq.index() as usize,
                        select: Box::new(|this: &mut Self, i| {
                            this.shared.eq.store(i as u8, Ordering::Relaxed)
                        }),
                        options: [t.eq_front, t.eq_diffuse, t.eq_none]
                            .into_iter()
                            .map(|label| RadioItem {
                                label: label.into(),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: t.quit.into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| {
                    this.shared.quit.store(true, Ordering::Relaxed)
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Show the tray icon and keep its status text fresh. Returns quietly if no
/// tray host is available.
pub fn spawn(shared: Arc<Shared>) {
    let tray = SpatialTray {
        shared: shared.clone(),
        text: if chinese() { &ZH } else { &EN },
    };
    let handle = match tray.spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("tray unavailable: {e}");
            return;
        }
    };
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            while !shared.quit.load(Ordering::Relaxed) && !handle.is_closed() {
                // Re-evaluate tooltip, status and menu.
                handle.update(|_| {});
                std::thread::sleep(Duration::from_secs(1));
            }
            let _ = handle.shutdown();
        })
        .ok();
}
