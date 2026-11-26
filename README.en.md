# SpatialAudioLinux

[中文](README.md)

Head-tracked spatial audio on Linux using the head tracker built into your headphones. It reads the headphones' IMU and renders a 7.1 virtual speaker layout binaurally on PipeWire, so sounds stay put in the room when you turn your head.

On phones, "spatial audio with head tracking" is rendered on the phone; the headphones only report head orientation. This project does that rendering on the PC.

## Supported headphones

Any headphones implementing Google's [Android Head Tracker HID protocol](https://source.android.com/docs/core/interaction/sensors/sensor-types#head_tracker). Devices are detected by protocol, not by vendor: a HID sensor collection (usage page `0x20`, usage `0xE1`) whose sensor description starts with `#AndroidHeadTracker#`. Both Bluetooth Classic HID and BLE HID (HOGP) work.

| Headphones | Status |
| --- | --- |
| Sony WF-1000XM5 | Tested |
| Sony WH-1000XM5 / WH-1000XM6 / WF-1000XM6, ULT WEAR and other Sony models with head tracking | Should work, reports welcome |
| Other headphones supporting Android spatial audio head tracking | Should work, reports welcome |

Headphones with proprietary protocols (e.g. AirPods) are not supported. Some headphones only advertise the HID service after a firmware update or after enabling head tracking in the vendor's phone app.

## How it works

```
apps ──► "Spatial Audio (Head Tracking)" virtual 7.1 sink
              │  one virtual speaker per channel, HRTF picked per head pose (SOFA),
              │  uniformly partitioned FFT convolution, crossfaded filter updates
              ▼
         stereo playback stream ──► headphones (the Bluetooth output of the tracking headset)
              ▲
  hidraw ◄── head tracker (Android Head Tracker HID; the HID channel is connected via BlueZ if needed)
```

- Speakers follow ITU 7.1: front ±30°, centre 0°, sides ±90°, rears ±150°. LFE bypasses the HRTF. Stereo content plays from the front pair.
- The default HRTF is libmysofa's MIT KEMAR (`default.sofa`); use `--sofa` for any SOFA file.
- Raw HRTF measurements carry the colouration of the measurement chain: MIT KEMAR is measured at the ear canal of a dummy head, so every direction shares a +15 dB ear-canal resonance at 2-4 kHz and the measurement loudspeaker removes 20-35 dB of bass; played directly it sounds like a telephone. A minimum-phase equaliser (4096 taps) on the output fixes this: `front` equalises to the front ±30° speakers so stereo content keeps its timbre, `diffuse` is standard diffuse-field equalisation, `none` disables it.
- Both streams share a node group and link group (like `pw-loopback`), so no feedback loops.
- Processing runs at 48 kHz; PipeWire converts other rates.
- After the sensor is enabled, its attitude takes a while to converge to gravity (about 15 s on the WF-1000XM5). Sound stays in front meanwhile; once the readings settle, the current direction becomes the front automatically.
- Recentring assumes you look straight ahead with a level head: only the heading is kept, and the tilt of the earbud in your ear is stored as a fixed offset. Run `recenter` while looking ahead whenever the front feels off.
- Auto-recentring follows Android's `HeadTrackingProcessor` with the same parameters: when the head stays within 10.5° for 6 seconds, that heading becomes the front. Unlike Android only the heading changes, the mount calibration is kept, so looking down at something is not mistaken for level. The front moves at most 0.8 rad/s, so it never jumps.

## Install

### NixOS (flake)

```nix
{
  inputs.spatial-audio-linux.url = "github:xkeyC/SpatialAudioLinux";

  # in your NixOS configuration
  imports = [ inputs.spatial-audio-linux.nixosModules.default ];
  programs.spatial-audio-linux.enable = true;
  # programs.spatial-audio-linux.extraArgs = [ "--no-auto-recenter" ];
}
```

The module installs the program and udev rule and registers the `spatial-audio-linux` user service (started with the graphical session).

### Other distributions

Requires Rust, `pkg-config`, PipeWire development files, clang (for bindgen) and libmysofa's `default.sofa`.

```sh
cargo build --release
sudo install -Dm755 target/release/spatial-audio-linux /usr/local/bin/spatial-audio-linux
# udev: give the logged-in user access to head-tracker hidraw nodes only
echo 'ACTION=="add|change", SUBSYSTEM=="hidraw", PROGRAM=="/usr/local/bin/spatial-audio-linux is-head-tracker %S%p", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-spatial-audio-linux.rules
sudo udevadm control --reload && sudo udevadm trigger -s hidraw
```

The rule file number must be below 73 for `uaccess` to take effect.

## Usage

```sh
spatial-audio-linux            # same as `run`: create the virtual sink and render
spatial-audio-linux list       # list head trackers and Bluetooth headsets offering HID
spatial-audio-linux monitor    # print the head pose only (no audio), to check the tracker
spatial-audio-linux recenter   # make the current head direction the front
spatial-audio-linux toggle     # head tracking on/off (off: speakers follow the head)
spatial-audio-linux status
```

After starting, select "Spatial Audio (Head Tracking)" as the default output in your sound settings. A headphones icon appears in the system tray: left click recenters, middle click toggles head tracking, and the menu shows the status and switches head tracking, auto-recentring and the equaliser. Use `--no-tray` to hide it. Binding `spatial-audio-linux recenter` to a global shortcut is recommended.

Common options (see `spatial-audio-linux run --help`):

| Option | Meaning |
| --- | --- |
| `--sofa <file>` | HRTF data (SOFA); also `SPATIAL_AUDIO_SOFA` |
| `--eq front\|diffuse\|none` | Headphone equalisation of the HRTFs, default `front` (see above); also in the tray |
| `--no-auto-recenter` | Disable auto-recentring (see above) |
| `--front-angle <deg>` | Angle of the front left/right speakers, default 30 |
| `--gain-db`, `--lfe-gain-db` | Output and LFE gain |
| `--target <node>` | Output node (node.name or object.serial); default follows the tracking headset |
| `--device <hidraw or MAC>` | Pick a tracker when several are present |
| `--invert` | Use if sounds turn with your head instead of staying put |

## Limitations

- Anechoic HRTFs only, no room reverb; externalisation depends on how well the HRTF matches your ears.
- No magnetometer, so yaw drifts slowly; auto-recentring corrects it, or run `recenter`.
- One head tracker at a time.

## License

GPL-3.0-or-later
