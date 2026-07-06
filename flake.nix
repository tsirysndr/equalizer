{
  description = "equalizer - a real-time terminal equalizer for raw PCM pipes (Rockbox DSP + ratatui)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Current crane doesn't expose a `nixpkgs` input, so we don't follow it.
    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-analyzer-src.follows = "";
    };

    flake-utils.url = "github:numtide/flake-utils";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, crane, fenix, flake-utils, advisory-db, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };

        inherit (pkgs) lib;

        craneLib = crane.mkLib pkgs;

        # cleanCargoSource alone would drop the gRPC inputs: keep the
        # .proto sources (tonic-build codegen) and the committed
        # descriptor.bin (include_bytes! for server reflection).
        src = lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            (lib.hasSuffix ".proto" path)
            || (lib.hasSuffix ".bin" path)
            || (craneLib.filterCargoSources path type);
          name = "source";
        };

        # The rockbox-dsp crate vendors and compiles the Rockbox DSP C
        # sources via `cc` (stdenv provides the compiler). cpal links
        # CoreAudio on Darwin automatically and ALSA on Linux via
        # pkg-config.
        commonArgs = {
          inherit src;

          pname = "equalizer";
          version = "0.1.0";
          strictDeps = true;

          nativeBuildInputs = [
            pkgs.pkg-config
            # tonic-build regenerates the gRPC API from proto/ at build time.
            pkgs.protobuf
          ] ++ lib.optionals pkgs.stdenv.isDarwin [
            # coreaudio-sys generates its CoreAudio bindings with bindgen at
            # build time; bindgenHook provides libclang (LIBCLANG_PATH) and
            # points clang at the Nix Apple SDK headers.
            pkgs.rustPlatform.bindgenHook
          ];

          buildInputs = lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ] ++ lib.optionals pkgs.stdenv.isLinux [
            # cpal links against ALSA on Linux for the audio output path.
            pkgs.alsa-lib
          ];

          cargoExtraArgs = "--locked --bin equalizer";

          meta = {
            description = "A real-time terminal equalizer for raw PCM pipes — Rockbox 10-band EQ with a Synthwave '84 ratatui UI";
            homepage = "https://github.com/tsirysndr/equalizer";
            # The bundled Rockbox DSP firmware code is GPL-2.0-or-later.
            license = lib.licenses.gpl2Plus;
            mainProgram = "equalizer";
            platforms = lib.platforms.unix;
          };
        };

        craneLibLLvmTools = craneLib.overrideToolchain
          (fenix.packages.${system}.complete.withComponents [
            "cargo"
            "llvm-tools"
            "rustc"
          ]);

        # Cache the dependency graph separately from the crate source.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        equalizer = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          doCheck = false;
        });

      in
      {
        checks = {
          inherit equalizer;

          equalizer-clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          equalizer-doc = craneLib.cargoDoc (commonArgs // {
            inherit cargoArtifacts;
          });

          equalizer-fmt = craneLib.cargoFmt {
            inherit src;
          };

          equalizer-audit = craneLib.cargoAudit {
            inherit src advisory-db;
          };

          equalizer-nextest = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
          });
        } // lib.optionalAttrs (system == "x86_64-linux") {
          equalizer-coverage = craneLib.cargoTarpaulin (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        packages = {
          default = equalizer;
          equalizer = equalizer;

          equalizer-llvm-coverage = craneLibLLvmTools.cargoLlvmCov (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        apps.default = flake-utils.lib.mkApp {
          drv = equalizer;
          name = "equalizer";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = builtins.attrValues self.checks.${system};

          # Build-time tools. pkg-config is required so cpal's build.rs can
          # resolve libasound on Linux.
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            pkg-config
            protobuf # protoc for tonic-build codegen
            ffmpeg # handy for piping test audio into the equalizer
            grpcurl # poke the control API by hand
          ];

          # Link-time libraries. Position matters: pkg-config only picks up
          # `.pc` files from `buildInputs`, so alsa-lib MUST live here (not
          # in nativeBuildInputs) for the cpal → ALSA link to resolve.
          buildInputs = with pkgs; lib.optionals stdenv.isDarwin [
            libiconv
          ] ++ lib.optionals stdenv.isLinux [
            alsa-lib
          ];

          shellHook = ''
            echo "🎛️ equalizer dev shell — try: ffmpeg -i track.flac -f s16le -ac 2 -ar 44100 - | cargo run --release"
          '';
        };
      });
}
