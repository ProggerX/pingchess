{
  description = "Chess and IRC-like rooms over ICMP echo packets";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs =
    { self, nixpkgs, crane }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      perSystem =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;

          craneLib = crane.mkLib pkgs;
          src = craneLib.cleanCargoSource ./.;

          commonArgs = {
            inherit src;
            strictDeps = true;
            pname = "pingchess";
            version = "0.1.0";
          };

          # Build dependencies once and reuse them for the package and checks.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          pingchess = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              doCheck = true;

              meta = {
                description = "Chess and IRC-like rooms over ICMP echo packets";
                mainProgram = "pingchess";
                platforms = lib.platforms.linux;
              };
            }
          );

          checks = {
            default = pingchess;

            pingchess-clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );

            pingchess-fmt = craneLib.cargoFmt {
              inherit src;
            };
          };
        in
        {
          packages = {
            default = pingchess;
            inherit pingchess;
          };

          apps.default = {
            type = "app";
            program = "${pingchess}/bin/pingchess";
          };

          inherit checks;

          devShells.default = craneLib.devShell {
            checks = checks;
            packages = with pkgs; [
              iproute2
              util-linux
              libcap
            ];
          };

          formatter = pkgs.nixfmt-rfc-style;
        };
    in
    {
      packages = forAllSystems (system: (perSystem system).packages);
      apps = forAllSystems (system: (perSystem system).apps);
      checks = forAllSystems (system: (perSystem system).checks);
      devShells = forAllSystems (system: (perSystem system).devShells);
      formatter = forAllSystems (system: (perSystem system).formatter);

      # Entry point for Hydra jobsets, e.g.
      # hydraJobs.x86_64-linux.pingchess. The aarch64 jobs need an aarch64
      # builder or a remote builder configured in Hydra.
      hydraJobs = forAllSystems (system: {
        pingchess = self.packages.${system}.pingchess;
        clippy = self.checks.${system}.pingchess-clippy;
        fmt = self.checks.${system}.pingchess-fmt;
      });
    };
}
