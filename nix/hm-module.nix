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
  configPath =
    if cfg.configFile != null then
      toString cfg.configFile
    else if settings != { } then
      "${home}/.local/state/openuur/config.toml"
    else
      null;

  home = config.home.homeDirectory;
  # systemd parses ExecStart itself: quotes are removed, then % specifiers
  # and $ variables are expanded. Shell quoting is not enough.
  escapeSystemdArg =
    arg: "\"" + lib.replaceStrings [ "\\" "\"" "$" "%" ] [ "\\\\" "\\\"" "$$" "%%" ] arg + "\"";
  escapeSystemdExecArgs = args: lib.concatMapStringsSep " " escapeSystemdArg args;
  # The daemon discovers the session's Wayland socket itself; snapshotting it
  # here would go stale after a logout or compositor restart.

  execArgs =
    [ "run" ]
    ++ lib.optionals (configPath != null) [
      "--config"
      configPath
    ]
    ++ cfg.extraArgs;

  environment =
    {
      HOME = home;
      XDG_CONFIG_HOME = "${home}/.config";
      XDG_STATE_HOME = "${home}/.local/state";
      XDG_RUNTIME_DIR = "%t";
      DBUS_SESSION_BUS_ADDRESS = "unix:path=%t/bus";
      RUST_LOG = "openuur=info,openuurd=info";
    }
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
    enable = lib.mkEnableOption "the openuur UU Remote host user service";

    package = lib.mkOption {
      type = types.package;
      default = pkgs.callPackage ./package.nix { ffmpeg = cfg.ffmpegPackage; };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "The openuur package to run.";
    };

    ffmpegPackage = lib.mkOption {
      type = types.package;
      default = pkgs.ffmpeg;
      description = "FFmpeg used by the capture pipeline.";
    };

    waylandDisplay = lib.mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Wayland socket name; detected at runtime when null.";
    };

    portalPersist = lib.mkOption {
      type = types.bool;
      default = true;
      description = "Try to persist portal permissions; falls back to DoNot on unsupported backends.";
    };

    configFile = lib.mkOption {
      type = types.nullOr (types.either types.path types.str);
      default = null;
      description = "Host config file, normally `~/.local/state/openuur/config.toml`.";
    };

    settings = lib.mkOption {
      type = types.attrsOf types.anything;
      default = { };
      description = "Non-secret settings rendered to a read-only TOML config in the store.";
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
      description = "Capture source.";
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
      description = "Directory for received files.";
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
        description = "`UU_DISPLAY_FPS`.";
      };
      vaapiDevice = lib.mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        description = "`UU_VAAPI_DEVICE`.";
      };
      ffmpeg = lib.mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        description = "`UU_FFMPEG`.";
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
        description = "`UU_AUDIO_SOURCE`.";
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
      description = "Extra environment variables.";
    };

    environmentFile = lib.mkOption {
      type = types.nullOr (types.either types.path types.str);
      default = null;
      description = "systemd user EnvironmentFile.";
    };

    extraArgs = lib.mkOption {
      type = types.listOf types.str;
      default = [ ];
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
      description = "Extra systemd user service settings.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !(cfg.configFile != null && settings != { });
        message = "services.openuur.configFile cannot be combined with settings/displaySource/fileSavePath; put those keys in the config file.";
      }
      {
        assertion = !(settings ? token);
        message = "do not put token in services.openuur.settings; use configFile for credentials.";
      }
    ];

    # Copy the generated non-secret config once. Later registration/token
    # updates stay in the writable state file; remove it to re-apply settings.
    home.activation.openuurSettings = lib.mkIf (settings != { }) (
      lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        if [ ! -e "${configPath}" ]; then
          mkdir -p "$(dirname "${configPath}")"
          cp ${generatedConfig} "${configPath}"
          chmod 600 "${configPath}"
        fi
      ''
    );

    systemd.user.services.openuurd = {
      Unit = {
        Description = "openuur UU Remote host";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };

      Service =
        {
          Type = "simple";
          ExecStart = "${cfg.package}/bin/openuurd ${escapeSystemdExecArgs execArgs}";
          Restart = cfg.restart;
          RestartSec = cfg.restartSec;
          WorkingDirectory = home;
          Environment = lib.mapAttrsToList (name: value: "${name}=${value}") environment;
        }
        // lib.optionalAttrs (cfg.environmentFile != null) {
          EnvironmentFile = toString cfg.environmentFile;
        }
        // cfg.extraServiceConfig;

      Install.WantedBy = [ "default.target" ];
    };
  };
}
