{ lib, fetchurl, stdenv }:

stdenv.mkDerivation rec {
  name = "grcov";
  version = "0.5.4";

  src = fetchurl {
    url = "https://github.com/mozilla/grcov/releases/download/v0.5.4/grcov-linux-x86_64.tar.bz2";
    sha256 = "0hqplgj3i8js42v2kj44khk543a93sk3n6wlfpv3c84pdqlm29br";
  };

  buildPhase = "";
  installPhase = ''
    mkdir -p $out/bin
    ls -lah
    cp foo $out/bin
  '';
}
