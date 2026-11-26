//! User settings changed from the tray or the control socket, persisted in
//! `$XDG_CONFIG_HOME/spatial-audio-linux/settings.conf`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::eq::EqMode;
use crate::tracking::Shared;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub head_tracking: bool,
    pub auto_recenter: bool,
    pub equaliser: EqMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            head_tracking: true,
            auto_recenter: true,
            equaliser: EqMode::Front,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("spatial-audio-linux").join("settings.conf"))
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

impl Settings {
    pub fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "head_tracking" => s.head_tracking = parse_bool(value).unwrap_or(s.head_tracking),
                "auto_recenter" => s.auto_recenter = parse_bool(value).unwrap_or(s.auto_recenter),
                "equaliser" => {
                    if let Some(m) = EqMode::ALL.into_iter().find(|m| m.name() == value) {
                        s.equaliser = m;
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn render(&self) -> String {
        format!(
            "# spatial-audio-linux settings (written automatically)\n\
             head_tracking = {}\n\
             auto_recenter = {}\n\
             # front | diffuse | none\n\
             equaliser = {}\n",
            self.head_tracking,
            self.auto_recenter,
            self.equaliser.name()
        )
    }

    pub fn load() -> Settings {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| Settings::parse(&t))
            .unwrap_or_default()
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("conf.tmp");
        std::fs::write(&tmp, self.render())?;
        std::fs::rename(tmp, path)
    }

    pub fn from_shared(shared: &Shared) -> Settings {
        Settings {
            head_tracking: shared.tracking.load(Ordering::Relaxed),
            auto_recenter: shared.auto_recenter.load(Ordering::Relaxed),
            equaliser: EqMode::from_index(shared.eq.load(Ordering::Relaxed)),
        }
    }

    pub fn apply(&self, shared: &Shared) {
        shared.tracking.store(self.head_tracking, Ordering::Relaxed);
        shared
            .auto_recenter
            .store(self.auto_recenter, Ordering::Relaxed);
        shared.eq.store(self.equaliser.index(), Ordering::Relaxed);
    }
}

/// Save the settings whenever they change (checked once per second).
pub fn spawn_saver(shared: Arc<Shared>) {
    let mut saved = Settings::load();
    std::thread::Builder::new()
        .name("settings".into())
        .spawn(move || {
            loop {
                let quit = shared.quit.load(Ordering::Relaxed);
                let current = Settings::from_shared(&shared);
                if current != saved {
                    match current.save() {
                        Ok(()) => saved = current,
                        Err(e) => eprintln!("cannot save settings: {e}"),
                    }
                }
                if quit {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = Settings {
            head_tracking: false,
            auto_recenter: false,
            equaliser: EqMode::Diffuse,
        };
        assert_eq!(Settings::parse(&s.render()), s);
    }

    #[test]
    fn tolerates_junk() {
        let s = Settings::parse("equaliser = bogus\nhead_tracking = maybe\nfoo\n");
        assert_eq!(s, Settings::default());
    }
}
