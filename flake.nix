{
  description = "GroundEffect Linux development environment";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.gdalpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  outputs = { self, nixpkgs, gdalpkgs }: let
    system = "x86_64-linux";
    pkgs = import nixpkgs { inherit system; };
    geo = import gdalpkgs { inherit system; };
  in {
    devShells.${system}.default = pkgs.mkShell {
      packages = with pkgs; [
        cargo rustc rustfmt clippy gcc pkg-config go gnumake
      ] ++ [ geo.gdal (pkgs.python3.withPackages (ps: [ ps.pillow ])) ];
      nativeBuildInputs = [ pkgs.rustPlatform.bindgenHook ];
      GDAL_CONFIG = "${geo.gdal}/bin/gdal-config";
      GDAL_CACHEMAX = "64";
    };
  };
}
