{
  description = "openuur — UU Remote native Linux host and terminal client";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};
    in
    {
      packages = forAllSystems (system: {
        default = (pkgsFor system).callPackage ./nix/package.nix { };
        openuur = self.packages.${system}.default;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/openuur";
        };
        openuur = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/openuur";
        };
        openuurd = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/openuurd";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          lib = pkgs.lib;
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              pkg-config
              pipewire.dev
              libxkbcommon
              openssl
              clang
              llvmPackages.libclang
              ffmpeg
              wl-clipboard
            ];

            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            PKG_CONFIG_PATH = lib.makeSearchPath "lib/pkgconfig" [
              pkgs.pipewire.dev
              pkgs.openssl.dev
            ];
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.stdenv.cc.libc.dev}/include";
            # Cargo does not read Nix's NIX_LDFLAGS, so point rustc at the
            # libraries the wayland/keymap backend links against.
            LIBRARY_PATH = lib.makeLibraryPath [ pkgs.libxkbcommon ];
            LD_LIBRARY_PATH = lib.makeLibraryPath [ pkgs.libxkbcommon ];

            shellHook = ''
              echo "openuur dev shell"
              echo "  cargo test --all-targets"
              echo "  cargo clippy --all-targets"
            '';
          };
        }
      );

      nixosModules = {
        default = ./nix/module.nix;
        openuur = ./nix/module.nix;
      };

      homeManagerModules = {
        default = ./nix/hm-module.nix;
        openuur = ./nix/hm-module.nix;
      };

      # Alias used by some Home Manager configurations.
      homeModules = {
        default = ./nix/hm-module.nix;
        openuur = ./nix/hm-module.nix;
      };

      overlays.default =
        final: _prev:
        {
          openuur = final.callPackage ./nix/package.nix { };
        };

      checks = forAllSystems (system: {
        inherit (self.packages.${system}) default;
      });

      formatter = forAllSystems (system: (pkgsFor system).nixfmt-rfc-style);
    };
}
