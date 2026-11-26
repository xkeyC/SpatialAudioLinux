//! PipeWire side: a virtual multichannel sink feeding the renderer, and a
//! stereo playback stream that follows the head-tracking headphones.
//!
//! Like `pw-loopback`, the two streams share a node group (so they run in
//! the same graph cycle) and a link group (so the session manager never
//! loops the output back into our own sink).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use pw::spa::pod::Pod;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::render::{Engine, Speaker};
use crate::tracking::Shared;

const GROUP: &str = "spatial-audio-linux";
const MAX_INPUT: usize = 8;

pub struct AudioOptions {
    pub sample_rate: u32,
    pub partition_len: usize,
    /// node.name or object.serial of the output; follows the tracker if None.
    pub target: Option<String>,
    pub sink_name: String,
    pub description: String,
}

fn format_pod(format: AudioFormat, rate: u32, positions: &[u32]) -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(format);
    info.set_rate(rate);
    info.set_channels(positions.len() as u32);
    let mut pos = [0; MAX_CHANNELS];
    pos[..positions.len()].copy_from_slice(positions);
    info.set_position(pos);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serialize format")
    .0
    .into_inner()
}

struct CaptureData {
    engine: Engine,
    out: HeapProd<f32>,
    scratch: Vec<f32>,
}

struct PlaybackData {
    input: HeapCons<f32>,
    partition_len: usize,
    primed: bool,
}

#[derive(Clone)]
struct BluezSink {
    name: String,
    serial: String,
}

pub fn run(
    engine: Engine,
    speakers: &[Speaker],
    opts: AudioOptions,
    shared: Arc<Shared>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .context("cannot connect to PipeWire")?;
    let registry = core.get_registry_rc()?;

    let positions: Vec<u32> = speakers.iter().map(|s| s.position).collect();
    let position_names: Vec<&str> = speakers.iter().map(|s| s.name).collect();

    let ring = HeapRb::<f32>::new(opts.sample_rate as usize * 2);
    let (prod, cons) = ring.split();

    // Virtual sink that applications play into.
    let capture = pw::stream::StreamRc::new(
        core.clone(),
        "spatial-audio-sink",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => opts.sink_name.as_str(),
            *pw::keys::NODE_DESCRIPTION => opts.description.as_str(),
            *pw::keys::NODE_GROUP => GROUP,
            *pw::keys::NODE_LINK_GROUP => GROUP,
            *pw::keys::NODE_VIRTUAL => "true",
            "audio.position" => position_names.join(","),
            "device.icon-name" => "audio-headphones",
        },
    )?;
    let capture_data = CaptureData {
        engine,
        out: prod,
        scratch: vec![0.0; opts.partition_len * 2],
    };
    let _capture_listener = capture
        .add_local_listener_with_user_data(capture_data)
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let mut planes: [&[f32]; MAX_INPUT] = [&[]; MAX_INPUT];
            let n = datas.len().min(MAX_INPUT);
            for (plane, d) in planes.iter_mut().zip(datas.iter_mut()) {
                let offset = d.chunk().offset() as usize;
                let size = d.chunk().size() as usize;
                if let Some(bytes) = d.data() {
                    let end = (offset + size).min(bytes.len());
                    let bytes = &bytes[offset.min(end)..end];
                    // SAFETY: f32 has no invalid bit patterns; misaligned
                    // buffers are skipped below.
                    let (pre, floats, _) = unsafe { bytes.align_to::<f32>() };
                    if pre.is_empty() {
                        *plane = floats;
                    }
                }
            }
            let CaptureData {
                engine,
                out,
                scratch,
            } = data;
            engine.process(&planes[..n], |left, right| {
                for (i, (l, r)) in left.iter().zip(right).enumerate() {
                    scratch[2 * i] = *l;
                    scratch[2 * i + 1] = *r;
                }
                out.push_slice(&scratch[..left.len() * 2]);
            });
        })
        .register()?;

    // Stereo output to the headphones.
    let mut playback_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CLASS => "Stream/Output/Audio",
        *pw::keys::NODE_NAME => format!("{}.output", opts.sink_name),
        *pw::keys::NODE_DESCRIPTION => format!("{} output", opts.description),
        *pw::keys::NODE_GROUP => GROUP,
        *pw::keys::NODE_LINK_GROUP => GROUP,
        *pw::keys::NODE_PASSIVE => "true",
        "stream.dont-remix" => "true",
        "audio.position" => "FL,FR",
    };
    if let Some(target) = &opts.target {
        playback_props.insert(*pw::keys::TARGET_OBJECT, target.as_str());
    }
    let playback = pw::stream::StreamRc::new(core.clone(), "spatial-audio-output", playback_props)?;
    let playback_data = PlaybackData {
        input: cons,
        partition_len: opts.partition_len,
        primed: false,
    };
    let _playback_listener = playback
        .add_local_listener_with_user_data(playback_data)
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else {
                return;
            };
            let frames;
            {
                let Some(bytes) = d.data() else {
                    return;
                };
                // SAFETY: as above.
                let (pre, out, _) = unsafe { bytes.align_to_mut::<f32>() };
                if !pre.is_empty() {
                    return;
                }
                let max_frames = out.len() / 2;
                frames = if requested == 0 {
                    max_frames.min(1024)
                } else {
                    requested.min(max_frames)
                };
                let out = &mut out[..frames * 2];
                let part = data.partition_len;
                let avail = data.input.occupied_len() / 2;
                if !data.primed && avail >= frames + part {
                    data.primed = true;
                }
                if data.primed {
                    // Keep at most one partition of slack beyond this cycle.
                    let keep = frames + part;
                    if avail > keep + frames {
                        data.input.skip((avail - keep) * 2);
                    }
                    let got = data.input.pop_slice(out);
                    if got < out.len() {
                        out[got..].fill(0.0);
                        data.primed = false;
                    }
                } else {
                    out.fill(0.0);
                }
            }
            let chunk = d.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 8;
            *chunk.size_mut() = (frames * 8) as u32;
        })
        .register()?;

    let flags = pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS;
    let capture_format = format_pod(AudioFormat::F32P, opts.sample_rate, &positions);
    capture.connect(
        spa::utils::Direction::Input,
        None,
        flags,
        &mut [Pod::from_bytes(&capture_format).unwrap()],
    )?;
    let playback_format = format_pod(
        AudioFormat::F32LE,
        opts.sample_rate,
        &[
            spa::sys::SPA_AUDIO_CHANNEL_FL,
            spa::sys::SPA_AUDIO_CHANNEL_FR,
        ],
    );
    playback.connect(
        spa::utils::Direction::Output,
        None,
        flags,
        &mut [Pod::from_bytes(&playback_format).unwrap()],
    )?;

    // Follow the Bluetooth output of the head-tracking headphones.
    let sinks: Rc<RefCell<BTreeMap<u32, BluezSink>>> = Rc::default();
    let metadata: Rc<RefCell<Option<pw::metadata::Metadata>>> = Rc::default();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let sinks = sinks.clone();
            let metadata = metadata.clone();
            let registry = registry.downgrade();
            move |global| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        let name = props.get("node.name").unwrap_or_default();
                        if props.get("media.class") == Some("Audio/Sink")
                            && name.starts_with("bluez_output.")
                        {
                            sinks.borrow_mut().insert(
                                global.id,
                                BluezSink {
                                    name: name.to_string(),
                                    serial: props
                                        .get("object.serial")
                                        .unwrap_or_default()
                                        .to_string(),
                                },
                            );
                        }
                    }
                    pw::types::ObjectType::Metadata => {
                        if props.get("metadata.name") == Some("default")
                            && let Some(registry) = registry.upgrade()
                            && let Ok(m) = registry.bind::<pw::metadata::Metadata, _>(global)
                        {
                            *metadata.borrow_mut() = Some(m);
                        }
                    }
                    _ => {}
                }
            }
        })
        .global_remove({
            let sinks = sinks.clone();
            move |id| {
                sinks.borrow_mut().remove(&id);
            }
        })
        .register();

    let follow = opts.target.is_none();
    let current_target: RefCell<Option<String>> = RefCell::new(None);
    let timer = mainloop.loop_().add_timer({
        let playback = playback.clone();
        let shared = shared.clone();
        let mainloop = mainloop.clone();
        move |_| {
            if shared.quit.load(Ordering::Relaxed) {
                mainloop.quit();
                return;
            }
            if !follow {
                return;
            }
            let node_id = playback.node_id();
            if node_id == spa::sys::SPA_ID_INVALID {
                return;
            }
            let address = shared.status.lock().unwrap().address.clone();
            let sinks = sinks.borrow();
            let wanted = address
                .map(|a| a.replace(':', "_"))
                .and_then(|mac| sinks.values().find(|s| s.name.contains(&mac)).cloned())
                .or_else(|| sinks.values().next().cloned());
            let Some(sink) = wanted else {
                return;
            };
            if current_target.borrow().as_deref() == Some(sink.name.as_str()) {
                return;
            }
            if let Some(m) = metadata.borrow().as_ref() {
                m.set_property(node_id, "target.object", Some("Spa:Id"), Some(&sink.serial));
                eprintln!("output -> {}", sink.name);
                *current_target.borrow_mut() = Some(sink.name);
            }
        }
    });
    timer.update_timer(
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(1)),
    );

    let _sigint = mainloop.loop_().add_signal_local(pw::loop_::Signal::INT, {
        let mainloop = mainloop.clone();
        move || mainloop.quit()
    });
    let _sigterm = mainloop.loop_().add_signal_local(pw::loop_::Signal::TERM, {
        let mainloop = mainloop.clone();
        move || mainloop.quit()
    });

    eprintln!(
        "sink \"{}\" ({}) ready, {} Hz, {} channels",
        opts.description,
        opts.sink_name,
        opts.sample_rate,
        speakers.len()
    );
    mainloop.run();
    shared.quit.store(true, Ordering::Relaxed);
    let _ = capture.disconnect();
    let _ = playback.disconnect();
    Ok(())
}
