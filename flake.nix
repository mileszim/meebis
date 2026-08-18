{
  description = "A fast, disposable, in-memory Redis-compatible server for ephemeral dev work";

  # nixpkgs is the only input on purpose. The per-system boilerplate that
  # flake-utils exists to remove is a dozen lines here, and not worth a
  # dependency that every consumer then has to fetch.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # The version lives in Cargo.toml, which release-please bumps. Reading it
      # here means the flake cannot drift out of step with a release.
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages = forAllSystems (pkgs: rec {
        meebis = pkgs.rustPlatform.buildRustPackage {
          pname = "meebis";
          version = cargoToml.package.version;

          src = pkgs.lib.cleanSourceWith {
            src = pkgs.lib.cleanSource ./.;
            # `target/` is hundreds of megabytes of build output that would
            # otherwise be copied into the store on every evaluation.
            filter = path: type: !(type == "directory" && baseNameOf path == "target");
          };

          # Vendoring straight from the lockfile: no `cargoHash` to recompute
          # and get wrong every time a dependency moves.
          cargoLock.lockFile = ./Cargo.lock;

          # meebis has one C dependency — the vendored Lua 5.1 sources behind
          # EVAL/EVALSHA — and stdenv already provides the compiler it needs.
          # Nothing else has to be declared.

          # Only the in-process unit tests. The integration suites shell out to
          # `sh`, bind sockets, and (for tests/compat) expect a real
          # `redis-server` to diff against; those belong in CI, where they run
          # on every pull request, rather than in a build sandbox.
          cargoTestFlags = [ "--bins" ];

          meta = {
            inherit (cargoToml.package) description;
            homepage = "https://github.com/mileszim/meebis";
            license = pkgs.lib.licenses.mit;
            mainProgram = "meebis";
            platforms = pkgs.lib.platforms.unix;
          };
        };
        default = meebis;
      });

      # For flakes that would rather add meebis to their own nixpkgs than
      # reference this one's outputs directly.
      overlays.default = final: _prev: {
        meebis = self.packages.${final.system}.meebis;
      };

      apps = forAllSystems (pkgs: rec {
        meebis = {
          type = "app";
          program = pkgs.lib.getExe self.packages.${pkgs.system}.meebis;
        };
        default = meebis;
      });

      # `nix flake check` builds the package.
      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.system}) meebis;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer

            # `tests/compat/run.sh` diffs meebis against a real redis-server
            # and needs both halves of the Redis distribution on PATH.
            redis

            # The RESP3 parity stage drives meebis through redis-py.
            (python3.withPackages (ps: [ ps.redis ]))
          ];

          shellHook = ''
            echo "meebis ${cargoToml.package.version} dev shell"
            echo "  cargo test                                       unit tests"
            echo "  bash tests/compat/run.sh ./target/release/meebis  Redis-spec compatibility"
          '';
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
