self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.spatial-audio-linux;
in
{
  options.programs.spatial-audio-linux = {
    enable = lib.mkEnableOption "SpatialAudioLinux（耳机头部追踪空间音频）";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "SpatialAudioLinux 包。";
    };
    autoStart = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "登录图形会话时自动启动用户服务 spatial-audio-linux。";
    };
    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "--no-auto-recenter" ];
      description = "传给 `spatial-audio-linux run` 的额外参数。";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    services.udev.packages = [ cfg.package ];

    systemd.user.services.spatial-audio-linux = {
      description = "SpatialAudioLinux 头部追踪空间音频";
      wantedBy = lib.optional cfg.autoStart "graphical-session.target";
      partOf = [ "graphical-session.target" ];
      after = [
        "graphical-session.target"
        "pipewire.service"
        "wireplumber.service"
      ];
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} run ${lib.escapeShellArgs cfg.extraArgs}";
        Restart = "on-failure";
        RestartSec = 3;
      };
    };
  };
}
