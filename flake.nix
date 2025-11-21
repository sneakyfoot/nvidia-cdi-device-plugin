{
  description = "Minimal NVIDIA CDI device plugin for Kubernetes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, ... }:
  let
    systems = [ "x86_64-linux" ];
    forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
  in
  {
    ###########################################################################
    # PACKAGES (binary + OCI image)
    ###########################################################################
    packages = forAllSystems (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        # ---- Binary ----
        nvidia-cdi-device-plugin = pkgs.buildGoModule {
          pname = "nvidia-cdi-device-plugin";
          version = "0.1.0";

          src = ./.;

          vendorHash = "sha256-3f/Y304K05HxFb1UMfAXeeo3sj5TD2dv84br+bqajtA=";
          ldflags = [ "-s" "-w" ];
        };

        # ---- OCI container image ----
        nvidia-cdi-device-plugin-image =
          pkgs.dockerTools.buildImage {
            name = "nvidia-cdi-device-plugin";
            tag = "0.1.0";
        
            copyToRoot = [
              (pkgs.buildEnv {
                name = "rootfs";
                paths = [
                  self.packages.${system}.nvidia-cdi-device-plugin
                ];
                # normalize the file layout so /bin exists
                pathsToLink = [ "/bin" ];
              })
            ];
        
            config = {
              Entrypoint = [ "/bin/nvidia-cdi-device-plugin" ];
            };
          };
      }
    );

    ###########################################################################
    # DEV SHELL
    ###########################################################################
    devShells = forAllSystems (system:
      let pkgs = import nixpkgs { inherit system; };
      in {
        default = pkgs.mkShell {
          buildInputs = [
            pkgs.go
            pkgs.gopls
            pkgs.go-tools
          ];
        };
      }
    );

    ###########################################################################
    # NixOS HOST MODULE (optional)
    ###########################################################################
    nixosModules.nvidia-cdi-device-plugin = { config, pkgs, lib, ... }:
    let
      cfg = config.services.nvidiaCdiDevicePlugin;
      inherit (lib) mkEnableOption mkIf mkOption types;
    in
    {
      options.services.nvidiaCdiDevicePlugin = {
        enable = mkEnableOption "NVIDIA CDI Kubernetes device plugin";

        package = mkOption {
          type = types.package;
          default = self.packages.${pkgs.system}.nvidia-cdi-device-plugin;
        };

        resourceName = mkOption {
          type = types.str;
          default = "nvidia.com/gpu";
        };

        kubeletDevicePluginDir = mkOption {
          type = types.path;
          default = "/var/lib/kubelet/device-plugins";
        };
      };

      config = mkIf cfg.enable {
        systemd.services.nvidia-cdi-device-plugin = {
          description = "NVIDIA CDI device plugin for Kubernetes";

          wants    = [ "kubelet.service" ];
          after    = [ "kubelet.service" "network-online.target" ];
          wantedBy = [ "multi-user.target" ];

          serviceConfig = {
            ExecStart =
              "${cfg.package}/bin/nvidia-cdi-device-plugin " +
              "--resource-name=${cfg.resourceName} " +
              "--kubelet-dir=${cfg.kubeletDevicePluginDir}";
            Restart = "always";
            RestartSec = 5;
          };
        };
      };
    };
  };
}
