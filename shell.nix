
{ pkgs ? import <nixpkgs> {} }:

let

  fenix = import (fetchTarball "https://github.com/nix-community/fenix/archive/main.tar.gz") { };
in

  pkgs.mkShell {

    shellHook = ''
      export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:${pkgs.lib.makeLibraryPath [
      pkgs.alsa-lib
      pkgs.udev
      pkgs.vulkan-loader
      pkgs.opusTools
      pkgs.pipewire
      pkgs.llvmPackages.libclang
    ]}"
      export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
    '';


    nativeBuildInputs = with pkgs; [
      pkg-config
    ];
    buildInputs = with pkgs; [

      (
        with fenix;
        combine (
          with default; [
            cargo
            clippy-preview
            latest.rust-src
            rust-analyzer
            rust-std
            rustc
            rustfmt-preview
          ]
          )
          )
          cargo-edit
          cargo-watch
          autoconf
          pkg-config
      opusTools
          cmake
          alsa-lib
          jack2
          pipewire

          lld
          clang
          llvmPackages.libclang

          udev
          #lutris
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
          vulkan-tools
          vulkan-headers
          vulkan-loader
          vulkan-validation-layers
          libjack2

    # # bevy-specific deps (from https://github.com/bevyengine/bevy/blob/main/docs/linux_dependencies.md)
  ];

}
