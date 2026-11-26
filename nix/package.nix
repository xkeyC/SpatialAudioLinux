{
  lib,
  rustPlatform,
  pkg-config,
  pipewire,
  libmysofa,
}:

let
  sofa = "${libmysofa}/share/libmysofa/default.sofa";
in
rustPlatform.buildRustPackage {
  pname = "spatial-audio-linux";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      !(lib.hasPrefix "result" (baseNameOf path))
      && baseNameOf path != "target"
      && baseNameOf path != "nix";
  };
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];
  buildInputs = [ pipewire ];

  env = {
    # 编进二进制的默认 HRTF（libmysofa 自带的 MIT KEMAR）
    SPATIAL_AUDIO_DEFAULT_SOFA = sofa;
    # 让 checkPhase 里的 HRTF 方向测试真正运行
    SPATIAL_AUDIO_SOFA = sofa;
  };

  postInstall = ''
    # 只给 Android Head Tracker 设备（按报告描述符判断）开放 hidraw 访问；
    # 必须排在 73-seat-late.rules 之前，uaccess 标签才会生效。
    install -Dm644 /dev/stdin $out/lib/udev/rules.d/70-spatial-audio-linux.rules <<RULES
    ACTION=="add|change", SUBSYSTEM=="hidraw", PROGRAM=="$out/bin/spatial-audio-linux is-head-tracker %S%p", TAG+="uaccess"
    RULES

    install -Dm644 /dev/stdin $out/lib/systemd/user/spatial-audio-linux.service <<UNIT
    [Unit]
    Description=Head-tracked spatial audio for PipeWire
    After=pipewire.service wireplumber.service
    PartOf=graphical-session.target

    [Service]
    ExecStart=$out/bin/spatial-audio-linux run
    Restart=on-failure
    RestartSec=3

    [Install]
    WantedBy=graphical-session.target
    UNIT
  '';

  meta = {
    description = "Head-tracked spatial audio for PipeWire using Android Head Tracker headphones";
    homepage = "https://github.com/xkeyC/SpatialAudioLinux";
    license = lib.licenses.gpl3Plus;
    platforms = lib.platforms.linux;
    mainProgram = "spatial-audio-linux";
  };
}
