{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.openuur;
  types = lib.types;
  toml = pkgs.formats.toml { };

  stripNulls = lib.filterAttrsRecursive (_name: value: value != null);
  settings = stripNulls (
    cfg.settings
    // lib.optionalAttrs (cfg.displaySource != null) {
      display_source = cfg.displaySource;
    }
    // lib.optionalAttrs (cfg.fileSavePath != null) {
      file_save_path = toString cfg.fileSavePath;
    }
  );
  generatedConfig = toml.generate "openuur-config.toml" settings;
  # `settings` is copied once to a writable state file so registration and
  # token refreshes can persist. Later edits to the Nix option need the state
  # file removed. `configFile` is used as-is for secret-managed configs.
  configPath =
    if cfg.configFile != null then
      toString cfg.configFile
    else if settings != { } then
      "${user.home}/.local/state/openuur/config.toml"
    else
      null;

  # The service must run as the account that owns the graphical session. The
  # assertion below rejects an enabled service without such an account.
  user =
    if cfg.user != null && builtins.hasAttr cfg.user config.users.users then
      config.users.users.${cfg.user}
    else
      {
        uid = 0;
        home = "/";
        name = "root";
      };
  runtimeDir = "/run/user/${toString user.uid}";

  # A system service does not inherit the compositor environment. The daemon
  # discovers the graphical session (and its Wayland socket) at runtime, so the
  # unit intentionally does not snapshot WAYLAND_DISPLAY or XDG_SESSION_TYPE.
  execArgs =
    [ "run" ]
    ++ lib.optionals (configPath != null) [
      "--config"
      configPath
    ]
    ++ cfg.extraArgs;

  baseEnvironment = {
    HOME = user.home;
    XDG_CONFIG_HOME = "${user.home}/.config";
    XDG_STATE_HOME = "${user.home}/.local/state";
    XDG_RUNTIME_DIR = runtimeDir;
    DBUS_SESSION_BUS_ADDRESS = "unix:path=${runtimeDir}/bus";
    RUST_LOG = "openuur=info,openuurd=info";
  };

  environment =
    baseEnvironment
    // lib.optionalAttrs (cfg.waylandDisplay != null) {
      WAYLAND_DISPLAY = cfg.waylandDisplay;
    }
    // lib.optionalAttrs (!cfg.portalPersist) {
      UU_PORTAL_PERSIST = "0";
    }
    // lib.optionalAttrs (cfg.video.encoder != "auto") {
      UU_VIDEO_ENCODER = cfg.video.encoder;
    }
    // lib.optionalAttrs (cfg.video.codec != "auto") {
      UU_VIDEO_CODEC = cfg.video.codec;
    }
    // lib.optionalAttrs (cfg.video.fps != null) {
      UU_DISPLAY_FPS = toString cfg.video.fps;
    }
    // lib.optionalAttrs (cfg.video.vaapiDevice != null) {
      UU_VAAPI_DEVICE = toString cfg.video.vaapiDevice;
    }
    // lib.optionalAttrs (cfg.video.ffmpeg != null) {
      UU_FFMPEG = toString cfg.video.ffmpeg;
    }
    // lib.optionalAttrs (!cfg.audio.enable) {
      UU_AUDIO = "0";
    }
    // lib.optionalAttrs (cfg.audio.source != null) {
      UU_AUDIO_SOURCE = cfg.audio.source;
    }
    // lib.optionalAttrs (!cfg.portMapping) {
      UU_PORT_MAPPING = "0";
    }
    // lib.optionalAttrs (!cfg.preventSleep) {
      UU_PREVENT_SLEEP = "0";
    }
    // lib.optionalAttrs (cfg.desktopBackend != "auto") {
      UU_DESKTOP_BACKEND = cfg.desktopBackend;
    }
    // cfg.environment;
in
{
  options.services.openuur = {
    enable = lib.mkEnableOption "the openuur UU Remote host";

    package = lib.mkOption {
      type = types.package;
      default = pkgs.callPackage ./package.nix { ffmpeg = cfg.ffmpegPackage; };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "The openuur package to run.";
    };

    ffmpegPackage = lib.mkOption {
      type = types.package;
      default = pkgs.ffmpeg;
      defaultText = lib.literalExpression "pkgs.ffmpeg";
      description = ''
        FFmpeg used by the capture pipeline. Use `pkgs.ffmpeg-full` when you
        need additional encoders such as NVENC.
      '';
    };

    user = lib.mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "alice";
      description = ''
        Account that owns the graphical Wayland session. The service runs as
        this account and reads its `~/.local/state/openuur/config.toml` unless
        `configFile` or `settings` is set.
      '';
    };

    group = lib.mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "users";
      description = "Group for the service. Defaults to the account's primary group.";
    };

    extraGroups = lib.mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "video"
        "render"
        "audio"
      ];
      description = "Supplementary groups for GPU, VAAPI, and audio access.";
    };

    waylandDisplay = lib.mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "wayland-1";
      description = ''
        Optional initial Wayland socket name. The daemon discovers the
        account's graphical session through logind and replaces this value at
        runtime; leave it null unless session discovery needs a fallback.
      '';
    };

    portalPersist = lib.mkOption {
      type = types.bool;
      default = true;
      description = ''
        Try to persist the portal permission and reuse a restore token. Some
        backends reject persistence for RemoteDesktop sessions; the host falls
        back to `DoNot` automatically. Set to false to skip the first attempt.
      '';
    };

    configFile = lib.mkOption {
      type = types.nullOr (types.either types.path types.str);
      default = null;
      example = "/run/secrets/openuur-config";
      description = ''
        Host config file. Use this for the account token and other secrets so
        they are not copied into the Nix store. Mutually exclusive with
        `settings`.
      '';
    };

    settings = lib.mkOption {
      type = types.attrsOf types.anything;
      default = { };
      example = {
        device_name = "living-room";
        shell = "bash";
      };
      description = ''
        Non-secret host settings rendered to a read-only TOML config in the
        Nix store. Do not put `token` here. Use `configFile` for credentials.
      '';
    };

    displaySource = lib.mkOption {
      type = types.nullOr (
        types.enum [
          "monitor"
          "window"
          "virtual"
        ]
      );
      default = null;
      description = "Capture source. Written to `display_source`.";
    };

    desktopBackend = lib.mkOption {
      type = types.enum [
        "auto"
        "portal"
        "x11"
        "wlroots"
      ];
      default = "auto";
      description = ''
        Capture backend (`UU_DESKTOP_BACKEND`). `auto` picks X11 for Xorg
        sessions, the portal for GNOME/KDE Wayland, and the native wlroots
        backend for other Wayland compositors.
      '';
    };

    preventSleep = lib.mkOption {
      type = types.bool;
      default = true;
      description = ''
        Inhibit system sleep and idle while any remote peer is connected
        (`UU_PREVENT_SLEEP=0` when disabled).
      '';
    };

    fileSavePath = lib.mkOption {
      type = types.nullOr (types.either types.path types.str);
      default = null;
      example = "/home/alice/Downloads";
      description = "Directory the remote file window opens on and receives into.";
    };

    video = {
      encoder = lib.mkOption {
        type = types.enum [
          "auto"
          "cpu"
          "nvenc"
          "vaapi"
          "qsv"
          "amf"
        ];
        default = "auto";
        description = "`UU_VIDEO_ENCODER`.";
      };
      codec = lib.mkOption {
        type = types.enum [
          "auto"
          "h264"
          "h265"
        ];
        default = "auto";
        description = "`UU_VIDEO_CODEC`.";
      };
      fps = lib.mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        description = "Advertised display refresh rate (`UU_DISPLAY_FPS`).";
      };
      vaapiDevice = lib.mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        example = "/dev/dri/renderD128";
        description = "`UU_VAAPI_DEVICE`.";
      };
      ffmpeg = lib.mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        description = "Explicit FFmpeg binary (`UU_FFMPEG`).";
      };
    };

    audio = {
      enable = lib.mkOption {
        type = types.bool;
        default = true;
        description = "Stream the default output's monitor as Opus.";
      };
      source = lib.mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor";
        description = "PulseAudio/PipeWire source (`UU_AUDIO_SOURCE`).";
      };
    };

    portMapping = lib.mkOption {
      type = types.bool;
      default = true;
      description = "Enable the official port-mapping tool.";
    };

    environment = lib.mkOption {
      type = types.attrsOf types.str;
      default = { };
      description = "Extra environment variables. Values must be strings.";
    };

    environmentFile = lib.mkOption {
      type = types.nullOr (types.either types.path types.str);
      default = null;
      example = "/run/secrets/openuur-env";
      description = "systemd EnvironmentFile, useful for `OPENUUR_TERMINAL_PASSWORD`.";
    };

    extraArgs = lib.mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "--verbose"
      ];
      description = "Extra arguments appended to `openuurd run`.";
    };

    restart = lib.mkOption {
      type = types.str;
      default = "on-failure";
      description = "systemd `Restart=` policy.";
    };

    restartSec = lib.mkOption {
      type = types.str;
      default = "3";
      description = "systemd `RestartSec=`.";
    };

    extraServiceConfig = lib.mkOption {
      type = types.attrsOf types.anything;
      default = { };
      description = "Extra systemd service settings merged into `systemd.services.openuurd`.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.user != null && builtins.hasAttr cfg.user config.users.users;
        message = "services.openuur.user must name an existing account that owns the graphical Wayland session.";
      }
      {
        assertion = !(cfg.configFile != null && settings != { });
        message = "services.openuur.configFile cannot be combined with settings/displaySource/fileSavePath; put those keys in the config file.";
      }
      {
        assertion = !(settings ? token);
        message = "do not put token in services.openuur.settings; use configFile for credentials.";
      }
    ];

    systemd.services.openuurd = {
      description = "openuur UU Remote host";
      after = [
        "network-online.target"
        "systemd-user-sessions.service"
      ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      environment = environment;

      serviceConfig =
        {
          Type = "simple";
          User = cfg.user;
          ExecStart = "${cfg.package}/bin/openuurd ${config.systemd.lib.escapeSystemdExecArgs execArgs}";
          Restart = cfg.restart;
          RestartSec = cfg.restartSec;
          WorkingDirectory = user.home;
          PrivateTmp = true;
          LimitNOFILE = 65536;
        }
        // lib.optionalAttrs (cfg.group != null) {
          Group = cfg.group;
        }
        // lib.optionalAttrs (cfg.extraGroups != [ ]) {
          SupplementaryGroups = cfg.extraGroups;
        }
        // lib.optionalAttrs (cfg.environmentFile != null) {
          EnvironmentFile = toString cfg.environmentFile;
        }
        // cfg.extraServiceConfig;
    };

    systemd.tmpfiles.rules =
      lib.optionals (settings != { }) [
        "d '${user.home}/.local/state/openuur' 0700 ${cfg.user} ${if cfg.group != null then cfg.group else "-"} - -"
        "C '${user.home}/.local/state/openuur/config.toml' 0600 ${cfg.user} ${if cfg.group != null then cfg.group else "-"} - ${generatedConfig}"
      ]
      ++ lib.optional (cfg.fileSavePath != null) ''
        d '${toString cfg.fileSavePath}' 0755 ${cfg.user} ${if cfg.group != null then cfg.group else "-"} - -
      '';
  };
}
