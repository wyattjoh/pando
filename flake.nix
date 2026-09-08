{
  description = "pando Git worktree manager";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-linux"
      ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forEachSystem (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "pando";
            version = "0.2.0";
            src = ./.;
            nativeBuildInputs = [
              pkgs.gitMinimal
              pkgs.which
              pkgs.zsh
            ]
            ++ pkgs.lib.optional pkgs.stdenv.isLinux pkgs.procps
            ++ pkgs.lib.optional pkgs.stdenv.isDarwin pkgs.unixtools.ps;
            preCheck = ''
              export HOME="$TMPDIR/home"
              mkdir -p "$HOME"
            '';
            cargoLock.lockFile = ./Cargo.lock;

            meta = {
              description = "Inspect and switch between Git worktrees";
              homepage = "https://github.com/wyattjoh/pando";
              license = pkgs.lib.licenses.mit;
              mainProgram = "pando";
            };
          };
        }
      );
    };
}
