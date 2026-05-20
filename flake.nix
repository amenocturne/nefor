{
  description = "More than an agentic harness";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSystem = f: nixpkgs.lib.genAttrs systems (system: f system);

      pluginNames = map (d: builtins.baseNameOf d) (
        builtins.filter (d: builtins.pathExists (d + "/Cargo.toml")) (
          let
            entries = builtins.readDir ./plugins;
          in
          map (name: ./plugins + "/${name}") (builtins.attrNames entries)
        )
      );

      mkNefor =
        system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs { inherit system overlays; };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rustfmt"
              "clippy"
              "rust-src"
            ];
          };

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          src = pkgs.lib.cleanSourceWith {
            src = craneLib.path ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || (builtins.match ".*\\.lua$" path != null)
              || (builtins.match ".*\\.md$" path != null)
              || (builtins.match ".*\\.jsonl$" path != null);
          };

          darwinDeps = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
            pkgs.apple-sdk_15
          ];

          linuxDeps = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.xorg.libxcb
          ];

          commonArgs = {
            inherit src;
            pname = "nefor";
            strictDeps = true;
            nativeBuildInputs = [ pkgs.git ];
            buildInputs = darwinDeps ++ linuxDeps;
            NEFOR_VERSION_OVERRIDE = self.shortRev or self.dirtyShortRev or "dev";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          pluginInstallCmds = builtins.concatStringsSep "\n" (
            map (name: ''
              if [ -f "$out/bin/${name}" ]; then
                mv "$out/bin/${name}" "$out/share/nefor/plugins/${name}"
              fi
            '') pluginNames
          );

          nefor = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              doCheck = false;

              postInstall = ''
                mkdir -p $out/share/nefor/plugins
                ${pluginInstallCmds}
                # Remove non-plugin, non-engine binaries (test harnesses, etc.)
                for bin in $out/bin/*; do
                  name=$(basename "$bin")
                  [ "$name" = "nefor" ] || rm -f "$bin"
                done
              '';

              meta = {
                description = "Agent harness substrate — NCP engine + plugins";
                homepage = "https://github.com/amenocturne/nefor";
                license = pkgs.lib.licenses.mit;
                mainProgram = "nefor";
              };
            }
          );
        in
        {
          inherit
            nefor
            craneLib
            darwinDeps
            linuxDeps
            pkgs
            ;
        };
    in
    {
      packages = forEachSystem (
        system:
        let
          n = mkNefor system;
        in
        {
          default = n.nefor;
          nefor = n.nefor;
        }
      );

      devShells = forEachSystem (
        system:
        let
          n = mkNefor system;
        in
        {
          default = n.craneLib.devShell {
            packages =
              with n.pkgs;
              [
                just
                cargo-nextest
                cargo-watch
              ]
              ++ n.darwinDeps
              ++ n.linuxDeps;
          };
        }
      );
    };
}
