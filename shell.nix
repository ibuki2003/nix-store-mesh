{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  packages = [];
  buildInputs = with pkgs; [
    openssl
    pkg-config
  ];
}
