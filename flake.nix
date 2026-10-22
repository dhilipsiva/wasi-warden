# wasi-warden's dev shell and package, cut from the portfolio template (products/templates/flake.nix).
#
#   nix develop        # Rust with the wasm32-wasip2 target, chromedriver for browser tasks
#   nix build .#hostd  # the host binary
#
# The toolchain lives in the flake, not in an ambient rustup.
{
  description = "wasi-warden: dev shell (pinned Rust with wasm32-wasip2, chromedriver) and the hostd package";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # An exact stable release. wasmtime 49 needs a recent one; move it deliberately.
        rustVersion = "1.99.1";

        rustToolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
          extensions = [
            "rust-src"
            "clippy"
            "rustfmt"
          ];
          # The agent component is a WASI Preview 2 component.
          targets = [ "wasm32-wasip2" ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # The host binary, built from Cargo.lock with the pinned toolchain. The agent component is
        # built separately (`cargo build -p agent-core --target wasm32-wasip2 --release`).
        hostd = rustPlatform.buildRustPackage {
          pname = "hostd";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [
            "-p"
            "hostd"
          ];
          nativeBuildInputs = [ pkgs.pkg-config ];
          # The end-to-end test needs the agent component and a fake LLM; CI runs it.
          doCheck = false;
          meta = {
            description = "wasi-warden host: runs an agent component with WIT-defined capabilities";
            license = with pkgs.lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "hostd";
          };
        };
      in
      {
        packages.hostd = hostd;
        packages.default = hostd;

        devShells.default = pkgs.mkShell {
          name = "wasi-warden";
          packages = [
            rustToolchain
            pkgs.pkg-config
            pkgs.just # the local gate (Justfile)
            pkgs.cargo-deny
            # Browser tasks talk to a WebDriver; chromedriver drives the Chromium it ships with.
            pkgs.chromedriver
            pkgs.chromium
            # A local OpenAI-compatible endpoint for the quick start (hostd.toml.example).
            pkgs.ollama
          ];
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          shellHook = ''
            echo "$(cargo --version) | $(chromedriver --version | head -1)"
          '';
        };
      }
    );
}
