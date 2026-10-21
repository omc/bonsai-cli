{
  description = "Command-line interface for Bonsai";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      workspace = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package;
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "bonsai";
          inherit (workspace) version;

          # Only what the build reads: `bonsai version` embeds openapi/v1.yml.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
              ./openapi
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [
            "-p"
            "bonsai"
          ];

          # aws-lc-sys builds with cmake.
          nativeBuildInputs = [ pkgs.cmake ];

          # The tests need the mock and a keychain; CI runs them.
          doCheck = false;

          meta = {
            description = "Command-line interface for Bonsai";
            homepage = "https://bonsai.io";
            license = pkgs.lib.licenses.asl20;
            mainProgram = "bonsai";
          };
        };
      });
    };
}
