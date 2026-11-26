# SpatialAudioLinux

[English](README.en.md)

在 Linux 上用耳机自带的头部追踪做空间音频：读取耳机里的陀螺仪，在 PipeWire 上把 7.1 声道渲染成双耳声。转头时声音留在原地，就像房间里摆着一圈音箱。

手机上的「空间音频 + 头部追踪」，渲染其实都在手机上完成，耳机只负责上报头部朝向。这个项目在电脑上补齐这一步。

## 支持的耳机

支持所有实现了 Google [Android Head Tracker HID 协议](https://source.android.com/docs/core/interaction/sensors/sensor-types#head_tracker)的耳机。程序不按品牌识别，只看两点：报告描述符里有 HID 传感器（Sensor 页 `0x20`、用途 `0xE1`），传感器描述以 `#AndroidHeadTracker#` 开头。传统蓝牙 HID 和低功耗蓝牙 HID（HOGP）都能用。

| 耳机 | 状态 |
| --- | --- |
| Sony WF-1000XM5 | 已测试 |
| Sony WH-1000XM5 / WH-1000XM6 / WF-1000XM6、ULT WEAR 等支持头部追踪的索尼型号 | 理论上支持，欢迎反馈 |
| 其他支持 Android 空间音频头部追踪的耳机 | 理论上支持，欢迎反馈 |

使用私有协议的耳机不支持，例如 AirPods。

有的耳机要先在厂商的手机 App 里升级固件、打开头部追踪，才会通告 HID 服务。

## 工作方式

```
应用 ──► 「Spatial Audio (Head Tracking)」虚拟 7.1 输出设备
              │  每个声道一个虚拟音箱，按头部朝向选 HRTF（SOFA 文件），
              │  分块 FFT 卷积，切换滤波器时交叉淡化
              ▼
         立体声播放流 ──► 耳机（自动找到带头追的那副蓝牙耳机）
              ▲
  hidraw ◄── 头部追踪（Android Head Tracker HID，必要时通过 BlueZ 主动连接 HID 通道）
```

- 虚拟音箱按 ITU 7.1 摆放：前左右 ±30°、中置 0°、侧 ±90°、后 ±150°。LFE 不过 HRTF，直接混到两耳。立体声应用的声音来自前方 ±30° 的两只音箱。
- HRTF 默认用 libmysofa 自带的 MIT KEMAR（`default.sofa`），可以用 `--sofa` 换成任意 SOFA 文件。
- 两条流同属一个 node.group 和 node.link-group（和 `pw-loopback` 一样），不会形成回环。
- 内部以 48 kHz 处理，其他采样率由 PipeWire 自动转换。

## 安装

### NixOS（flake）

```nix
{
  inputs.spatial-audio-linux.url = "github:xkeyC/SpatialAudioLinux";

  # 在 NixOS 配置里
  imports = [ inputs.spatial-audio-linux.nixosModules.default ];
  programs.spatial-audio-linux.enable = true;
  # programs.spatial-audio-linux.extraArgs = [ "--follow-secs" "30" ];
}
```

模块会安装程序和 udev 规则，并注册用户服务 `spatial-audio-linux`（登录后自动启动）。

### 其他发行版

需要 Rust、`pkg-config`、PipeWire 开发文件、clang（bindgen 用）和 libmysofa 的 `default.sofa`。

```sh
cargo build --release
sudo install -Dm755 target/release/spatial-audio-linux /usr/local/bin/spatial-audio-linux
# udev 规则：只给头追设备开放 hidraw 给当前登录用户
echo 'ACTION=="add|change", SUBSYSTEM=="hidraw", PROGRAM=="/usr/local/bin/spatial-audio-linux is-head-tracker %S%p", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-spatial-audio-linux.rules
sudo udevadm control --reload && sudo udevadm trigger -s hidraw
```

规则文件的编号必须小于 73，`uaccess` 才会生效。

## 使用

```sh
spatial-audio-linux            # 等同于 run：创建虚拟输出设备并开始渲染
spatial-audio-linux list       # 列出头追设备和带 HID 服务的蓝牙耳机
spatial-audio-linux monitor    # 只打印头部朝向（不出声），用来检查头追是否正常
spatial-audio-linux recenter   # 把当前朝向设为正前方
spatial-audio-linux toggle     # 开关头部追踪（关闭时音箱跟着头转）
spatial-audio-linux status
```

启动后，在系统的声音设置里把默认输出设为「Spatial Audio (Head Tracking)」。

建议给 `spatial-audio-linux recenter` 绑一个全局快捷键（KDE：系统设置 → 键盘 → 快捷键 → 添加命令）。

常用参数（`spatial-audio-linux run --help` 查看全部）：

| 参数 | 说明 |
| --- | --- |
| `--sofa <文件>` | HRTF 数据（SOFA）。也可以用环境变量 `SPATIAL_AUDIO_SOFA` 指定 |
| `--follow-secs <秒>` | 头静止时，正前方在这段时间内慢慢跟上头的朝向，用来抵消陀螺仪的偏航漂移；0 为关闭 |
| `--front-angle <度>` | 前置左右音箱的角度，默认 30 |
| `--gain-db`、`--lfe-gain-db` | 输出增益和 LFE 增益 |
| `--target <节点>` | 输出到指定节点（node.name 或 object.serial），默认跟随头追耳机 |
| `--device <hidraw 或 MAC>` | 有多个头追设备时指定用哪个 |
| `--invert` | 如果转头时声音跟着头走（方向反了），加上这个参数 |

## 已知限制

- 只有消声室 HRTF，没有房间混响，声音的「外化」感取决于 HRTF 与个人耳廓的匹配程度。
- 耳机没有磁力计，偏航角会慢慢漂移，需要时用 `recenter`，或者打开 `--follow-secs`。
- 同一时间只使用一个头追设备。

## 许可证

GPL-3.0-or-later
