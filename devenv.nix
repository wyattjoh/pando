{ pkgs, lib, ... }:

{
  languages.rust.enable = true;

  packages = [
    pkgs.prek
    pkgs.which
    pkgs.zsh
  ]
  ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.procps ]
  ++ lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.unixtools.ps ];

  enterTest = ''
    just lint
    just test
  '';
}
