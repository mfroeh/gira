{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      rust-overlay,
      nixpkgs,
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forEachSystem = f: map f supportedSystems;
      attrsetsDeepMerge = (with nixpkgs.lib; foldl (a: b: attrsets.recursiveUpdate a b) { });
    in
    forEachSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
      in
      {
        devShells.${system}.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
          ];
        };
      }
    ) |> attrsetsDeepMerge;
}
