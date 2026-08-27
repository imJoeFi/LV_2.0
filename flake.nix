{
  description = "Development and Raspberry Pi cross-compilation environment for LV 2.0";

  inputs = {
    fedimint.url = "github:fedimint/fedimint?rev=2620789610a2c65c1068de973ebb5657d08d549d";

    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { fedimint, nixpkgs, rust-overlay, ... }:
    let
      supportedSystems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.stable."1.92.0".default.override {
            targets = [ "aarch64-unknown-linux-gnu" ];
          };
        in
        {
          default = pkgs.mkShell {
            packages = [
              rustToolchain
              pkgs.cargo-zigbuild
              pkgs.clang
              pkgs.cmake
              pkgs.just
              pkgs.llvmPackages.libclang
              pkgs.pkg-config
              pkgs.zig
            ];

            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };

          e2e = fedimint.devShells.${system}.default.overrideAttrs (old: {
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
              fedimint.packages.${system}.devimint
              fedimint.packages.${system}.fedimint-pkgs
              fedimint.packages.${system}.gateway-pkgs
              pkgs.just
            ];
          });
        }
      );
    };
}
