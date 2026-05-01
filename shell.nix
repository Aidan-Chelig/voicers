{ pkgs ? import <nixpkgs> {} }:

let
  lib = pkgs.lib;
  stdenv = pkgs.stdenv;

  linuxRuntimeLibs = with pkgs; [
    alsa-lib
    udev
    vulkan-loader
    opus-tools
    pipewire
    llvmPackages.libclang
  ];

in
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    pkg-config
    cargo
    clippy
    rust-analyzer
    rustc
    rustfmt
    cargo-edit
    cargo-watch
    autoconf
    cmake
    clang
    llvmPackages.libclang
  ];

  buildInputs = with pkgs; [
    opus-tools
    lld
  ]
  ++ lib.optionals stdenv.isLinux (with pkgs; [
    alsa-lib
    jack2
    pipewire
    udev
    libxcursor
    libxrandr
    libxi
    vulkan-tools
    vulkan-headers
    vulkan-loader
    vulkan-validation-layers
    libjack2
  ])
  ++ lib.optionals stdenv.isDarwin (with pkgs; [
    apple-sdk
    libiconv
  ]);

  shellHook = ''
    export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
  ''
  + lib.optionalString stdenv.isLinux ''
    export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:${lib.makeLibraryPath linuxRuntimeLibs}"
  '';
}
