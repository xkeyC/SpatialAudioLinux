{
  description = "SpatialAudioLinux：用耳机的头部追踪（Android Head Tracker 协议）在 PipeWire 上渲染空间音频";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        spatial-audio-linux = pkgs.callPackage ./nix/package.nix { };
        default = spatial-audio-linux;
      });

      nixosModules.default = import ./nix/module.nix self;

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer pkg-config ];
          nativeBuildInputs = [ pkgs.rustPlatform.bindgenHook ];
          buildInputs = with pkgs; [ pipewire ];
          SPATIAL_AUDIO_SOFA = "${pkgs.libmysofa}/share/libmysofa/default.sofa";
        };
      });
    };
}
