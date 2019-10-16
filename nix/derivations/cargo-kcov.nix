{ lib, rustPlatform, fetchFromGitHub }:

rustPlatform.buildRustPackage rec {
  pname = "cargo-kcov";
  version = "0.5.2";

  src = fetchFromGitHub {
    owner = "kennytm";
    repo = "cargo-kcov";
    rev = "v${version}";
    sha256 = "0hqplgj3i8js42v2kj44khk543a93sk3n6wlfpv3c84pdqlm29br";
  };

  doCheck = false;
  cargoSha256 = "15ivifg70jg4cbvabgzndz4j60nccgnbnq2mdkc0ns65wxz4y610";
}
