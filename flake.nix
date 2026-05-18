{
  description = "NVIDIA CDI DRA driver for Kubernetes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, ... }:
  let
    inherit (nixpkgs) lib;
    systems = [ "x86_64-linux" ];
    forAllSystems = f: lib.genAttrs systems (system: f system);
  in
  {
    packages = forAllSystems (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        nvidia-cdi-device-plugin = pkgs.rustPlatform.buildRustPackage rec {
          pname = "nvidia-cdi-device-plugin";
          version = "0.2.2";

          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.protobuf ];
        };

        nvidia-cdi-device-plugin-image =
          pkgs.dockerTools.buildImage {
            name = "nvidia-cdi-device-plugin";
            tag = "0.2.2";

            copyToRoot = [
              (pkgs.buildEnv {
                name = "rootfs";
                paths = [
                  self.packages.${system}.nvidia-cdi-device-plugin
                  # CA bundle for talking to the kube-apiserver via HTTPS.
                  pkgs.cacert
                ];
                pathsToLink = [ "/bin" "/etc" ];
              })
            ];

            config = {
              Entrypoint = [ "/bin/nvidia-cdi-device-plugin" ];
              Env = [
                "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
                "RUST_LOG=info"
              ];
            };
          };
      }
    );

    devShells = forAllSystems (system:
      let pkgs = import nixpkgs { inherit system; };
      in {
        default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            pkg-config
            protobuf
          ];
        };
      }
    );

    nixosModules.nvidia-cdi-device-plugin = { config, pkgs, lib, ... }:
    let
      cfg = config.services.nvidiaCdiDraDriver;
      inherit (lib) mkEnableOption mkIf mkOption types;
    in
    {
      options.services.nvidiaCdiDraDriver = {
        enable = mkEnableOption "NVIDIA CDI DRA driver for Kubernetes";

        package = mkOption {
          type = types.package;
          default = self.packages.${pkgs.system}.nvidia-cdi-device-plugin;
        };

        driverName = mkOption {
          type = types.str;
          default = "gpu.nvidia.com";
        };

        deviceClass = mkOption {
          type = types.str;
          default = "gpu.nvidia.com";
        };

        extendedResourceName = mkOption {
          type = types.str;
          default = "nvidia.com/gpu";
        };

        kubeletDir = mkOption {
          type = types.path;
          default = "/var/lib/kubelet";
        };

        excludeDisplayGpus = mkOption {
          type = types.bool;
          default = false;
        };

        nvmlLibPath = mkOption {
          type = types.nullOr types.path;
          default = "/run/opengl-driver/lib/libnvidia-ml.so.1";
        };
      };

      config = mkIf cfg.enable {
        systemd.services.nvidia-cdi-dra-driver = {
          description = "NVIDIA CDI DRA driver for Kubernetes";

          wants    = [ "kubelet.service" ];
          after    = [ "kubelet.service" "network-online.target" ];
          wantedBy = [ "multi-user.target" ];

          environment = {
            NODE_NAME = config.networking.hostName;
            RUST_LOG = "info";
          };

          serviceConfig = {
            ExecStart = lib.concatStringsSep " " ([
              "${cfg.package}/bin/nvidia-cdi-device-plugin"
              "--driver-name=${cfg.driverName}"
              "--device-class=${cfg.deviceClass}"
              "--extended-resource-name=${cfg.extendedResourceName}"
              "--kubelet-dir=${cfg.kubeletDir}"
            ]
            ++ lib.optional cfg.excludeDisplayGpus "--exclude-display-gpus"
            ++ lib.optional (cfg.nvmlLibPath != null) "--nvml-lib-path=${cfg.nvmlLibPath}");
            Restart = "always";
            RestartSec = 5;
          };
        };
      };
    };
  };
}
