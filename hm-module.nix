{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.nefor;
  system = pkgs.stdenv.hostPlatform.system;
  neforPkgs = self.packages.${system};
in
{
  options.programs.nefor = {
    enable = lib.mkEnableOption "nefor agent harness";

    package = lib.mkOption {
      type = lib.types.package;
      default = neforPkgs.nefor;
      description = "The nefor package to use.";
    };

    starter = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Use the bundled starter configuration.
          Disable to provide your own via xdg.configFile."nefor".
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    home.sessionVariables = {
      NEFOR_DEV_DIR = "${neforPkgs.nefor-engine}";
      NEFOR_PLUGIN_DIR = "${cfg.package}/share/nefor/plugins";
    };

    xdg.configFile."nefor" = lib.mkIf cfg.starter.enable {
      source = neforPkgs.nefor-starter;
      recursive = true;
    };
  };
}
