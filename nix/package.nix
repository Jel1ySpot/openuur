{
  lib,
  rustPlatform,
  stdenv,
  pkg-config,
  makeWrapper,
  libxkbcommon,
  pipewire,
  llvmPackages,
  pam,
  ffmpeg,
  systemd,
  wireplumber,
  pulseaudio,
  wl-clipboard,
  xrandr,
  libei,
  glib,
  gtk3,
  qt6,
  procps,
  linuxHeaders,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
  runtimeTools = [
    ffmpeg
    systemd
    pipewire
    wireplumber
    pulseaudio
    wl-clipboard
    xrandr
    glib
    gtk3
    qt6.qttools
    procps
  ];
  runtimePath = lib.makeBinPath runtimeTools;
  # libpam and libei are dlopen()ed: auto-unlock and portal EIS must find them
  # without the package linking them at build time.
  runtimeLibraryPath = lib.makeLibraryPath [ pam libei ];
in
rustPlatform.buildRustPackage {
  pname = "openuur";
  version = cargoToml.package.version;

  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: _type:
      let
        base = builtins.baseNameOf (toString path);
      in
      !(builtins.elem base [
        ".git"
        ".direnv"
        "target"
        "result"
        "result-bin"
      ]);
  };

  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "--locked" ];

  nativeBuildInputs = [
    pkg-config
    makeWrapper
  ];

  buildInputs = [
    libxkbcommon
    pipewire
    pipewire.dev
    llvmPackages.libclang
    pam
    linuxHeaders
  ];

  # bindgen in pipewire-sys/libspa-sys needs libclang and libc headers.
  LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
  BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${stdenv.cc.libc.dev}/include -isystem ${linuxHeaders}/include";

  # The host shells out to ffmpeg, loginctl, pactl/wpctl, wl-copy/wl-paste and
  # gio/gtk-launch. The wrapper keeps those tools available even when the
  # service starts with a minimal PATH. libpam and libei are dlopen()ed at
  # runtime, so they must be on LD_LIBRARY_PATH in the Nix store.
  postInstall = ''
    for program in openuur openuurd; do
      wrapProgram "$out/bin/$program" \
        --prefix PATH : "${runtimePath}" \
        --suffix LD_LIBRARY_PATH : "${runtimeLibraryPath}"
    done
  '';

  # The test suite spawns PTYs, shells and local WebRTC sockets. The Nix
  # sandbox has no usable environment for those integration tests, so run
  # `nix develop` and `cargo test --all-targets` instead.
  doCheck = false;

  meta = {
    description = "UU Remote native Linux host and terminal client";
    license = lib.licenses.mit;
    mainProgram = "openuur";
    platforms = lib.platforms.linux;
  };
}
