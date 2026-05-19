{
  description = "nefor — agent harness substrate";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forEachSystem = f: nixpkgs.lib.genAttrs systems (system: f system);

      mkNefor = system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs { inherit system overlays; };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rustfmt" "clippy" "rust-src" ];
          };

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          src = pkgs.lib.cleanSourceWith {
            src = craneLib.path ./.;
            filter = path: type:
              (craneLib.filterCargoSources path type)
              || (builtins.match ".*\\.lua$" path != null)
              || (builtins.match ".*\\.md$" path != null);
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

          nefor = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;

            postInstall = ''
              mkdir -p $out/share/nefor/plugins
              for bin in \
                basic-tools chatgpt-provider generic-provider generic-tool \
                mock-plugin nefor-combinators nefor-tui openai-provider \
                reasoner-graph tool-gate; do
                if [ -f "$out/bin/$bin" ]; then
                  mv "$out/bin/$bin" "$out/share/nefor/plugins/$bin"
                fi
              done
              rm -f "$out/bin/fake-engine"
            '';

            meta = {
              description = "Agent harness substrate — NCP engine + plugins";
              homepage = "https://github.com/amenocturne/nefor";
              license = pkgs.lib.licenses.mit;
              mainProgram = "nefor";
            };
          });
        in
        {
          inherit nefor cargoArtifacts commonArgs craneLib src darwinDeps linuxDeps pkgs;

          checks = {
            inherit nefor;
            nefor-clippy = craneLib.cargoClippy (commonArgs // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--workspace --all-targets -- -D warnings";
            });
            nefor-fmt = craneLib.cargoFmt { inherit src; pname = "nefor"; };
            nefor-tests = craneLib.cargoNextest (commonArgs // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
            });
          };
        };
    in
    {
      checks = forEachSystem (system: (mkNefor system).checks);

      packages = forEachSystem (system:
        let n = mkNefor system; in {
          default = n.nefor;
          nefor = n.nefor;
        });

      devShells = forEachSystem (system:
        let n = mkNefor system; in {
          default = n.craneLib.devShell {
            checks = self.checks.${system};
            packages = with n.pkgs; [
              just
              cargo-nextest
              cargo-watch
            ] ++ n.darwinDeps ++ n.linuxDeps;
          };
        });
    };
}
