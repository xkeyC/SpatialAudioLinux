//! Control socket: `recenter`, `toggle`, `on`, `off` and `status`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};

use crate::tracking::Shared;

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    dir.join("spatial-audio-linux.sock")
}

pub fn serve(shared: Arc<Shared>) -> Result<()> {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        anyhow::bail!("another instance is already running ({})", path.display());
    }
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &shared);
            }
        })?;
    Ok(())
}

fn handle(stream: UnixStream, shared: &Shared) -> Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let mut out = &stream;
    let reply = match line.trim() {
        "recenter" => {
            shared.recenter.store(true, Ordering::Relaxed);
            "ok".to_string()
        }
        "on" => {
            shared.tracking.store(true, Ordering::Relaxed);
            "head tracking on".to_string()
        }
        "off" => {
            shared.tracking.store(false, Ordering::Relaxed);
            "head tracking off".to_string()
        }
        "toggle" => {
            let on = !shared.tracking.fetch_xor(true, Ordering::Relaxed);
            format!("head tracking {}", if on { "on" } else { "off" })
        }
        "status" => {
            let st = shared.status.lock().unwrap().clone();
            let tracking = shared.tracking.load(Ordering::Relaxed);
            match st.device {
                Some(dev) => format!(
                    "tracker: {dev} ({}{})\nprotocol: {}\nhead tracking: {}\nsamples: {}\nfront source: azimuth {:.1}°, elevation {:.1}°",
                    st.devnode.unwrap_or_default(),
                    st.address.map(|a| format!(", {a}")).unwrap_or_default(),
                    st.protocol.unwrap_or_default(),
                    if tracking { "on" } else { "off" },
                    st.samples,
                    st.front_azimuth,
                    st.front_elevation,
                ),
                None => format!(
                    "tracker: none\nhead tracking: {}",
                    if tracking { "on" } else { "off" }
                ),
            }
        }
        other => format!("unknown command {other:?}"),
    };
    writeln!(out, "{reply}")?;
    Ok(())
}

pub fn send(command: &str) -> Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("spatial-audio-linux is not running ({})", path.display()))?;
    writeln!(stream, "{command}")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    std::io::Read::read_to_string(&mut stream, &mut reply)?;
    Ok(reply.trim_end().to_string())
}
