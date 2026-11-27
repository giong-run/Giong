### Added

- **giong-cli**: release binaries come with a signed build provenance attestation (SLSA,
  through GitHub artifact attestations) and a CycloneDX SBOM (`giong-<version>-<target>.cdx.json`
  on the Release), for each archive and for the binary in it. `gh attestation verify
  ~/.giong/bin/giong --repo giong-run/Giong --signer-workflow
  giong-run/Giong/.github/workflows/release.yml` shows that an installed `giong` was built
  by the release workflow from the tagged commit. The binary also lists the crates
  compiled into it (cargo auditable, about 400 bytes), so `cargo audit bin`, Trivy, Grype
  or osv-scanner can check it for known advisories.
