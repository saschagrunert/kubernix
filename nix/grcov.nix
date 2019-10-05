{ lib, fetchurl, stdenv }:

stdenv.mkDerivation rec {
  name = "grcov";
  version = "0.5.4";

  src = fetchurl {
    url = "https://github.com/mozilla/grcov/releases/download/v0.5.4/grcov-linux-x86_64.tar.bz2";
    sha256 = "0p5zry2p7mcfsk7zrrb0ccivf3r666j2hs9c1x2rzwybx1ainfqs";
  };

  buildPhase = "";
  installPhase = ''
    mkdir -p $out/bin
    ls -lah
    cp foo $out/bin
  '';
}
