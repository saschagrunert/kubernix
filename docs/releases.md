# Verifying Releases

Every release ships the `kubernix-x86_64` and `kubernix-aarch64` binaries, an
SPDX 2.3 SBOM per binary (`kubernix-<arch>.spdx.json`) and a `SHA256SUMS` file
covering all of them. `SHA256SUMS` is signed keyless with
[cosign](https://docs.sigstore.dev/cosign/) and the signature is published as
`SHA256SUMS.sigstore.json`. To verify a downloaded binary, use cosign v3. Older
cosign v2 releases need `--new-bundle-format` in addition and may fail on
bundles with Rekor v2 entries:

```shell
$ cosign verify-blob SHA256SUMS \
    --bundle SHA256SUMS.sigstore.json \
    --certificate-identity-regexp '^https://github\.com/saschagrunert/kubernix/\.github/workflows/ci\.yml@refs/tags/v' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
$ sha256sum -c SHA256SUMS --ignore-missing
```

The binaries also carry GitHub build provenance and SBOM attestations:

```shell
$ gh attestation verify kubernix-x86_64 --repo saschagrunert/kubernix
$ gh attestation verify kubernix-x86_64 --repo saschagrunert/kubernix \
    --predicate-type https://spdx.dev/Document/v2.3
```

Whether known vulnerabilities in dependencies affect KuberNix is tracked in the
[OpenVEX](https://openvex.dev) document
[`.openvex/kubernix.openvex.json`](../.openvex/kubernix.openvex.json). It is a
living document, not a release asset, so scanners should use its current version
from the `main` branch:

```shell
$ curl -sSfLO https://raw.githubusercontent.com/saschagrunert/kubernix/main/.openvex/kubernix.openvex.json
$ grype sbom:kubernix-x86_64.spdx.json --vex kubernix.openvex.json
$ trivy sbom --vex kubernix.openvex.json kubernix-x86_64.spdx.json
```

Each statement uses the versioned package URL of the affected dependency as
product, so it applies as long as that dependency version ships and ages out
when it gets bumped. The advisory needs all of its IDs as name and aliases,
since grype matches the GHSA and trivy the CVE ID. This form was verified with
grype v0.120.0 and trivy v0.76.1 scanning the release SBOM as above, both ignore
a `pkg:cargo/kubernix` product with the dependency as subcomponent. Statements
get added with [`vexctl`](https://github.com/openvex/vexctl) and checked with
`make lint-vex`. For example, a statement for an advisory of the
`crossbeam-channel` crate (which is not a dependency anymore) would look like
this:

```shell
$ vexctl add --in-place .openvex/kubernix.openvex.json \
    --product pkg:cargo/crossbeam-channel@0.5.12 \
    --vuln RUSTSEC-2025-0024 \
    --aliases CVE-2025-4574,GHSA-pg9f-39pc-qf8g \
    --status not_affected \
    --justification vulnerable_code_not_in_execute_path
```
