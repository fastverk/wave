fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Messages only (prost-build, not tonic-build — the wave proto has no
    // service, so no service generator and no runtime tonic dep). The proto is
    // self-contained (its own RepoRef/ChangeRef), so there's no forge import to
    // route through extern_path — wave-core converts to/from the forge runtime
    // types at the forge edge instead (keeps wave's @crates prost independent of
    // forge's across the Bazel module boundary).
    prost_build::Config::new().compile_protos(&["../proto/wave/v1/wave.proto"], &["../proto"])?;
    Ok(())
}
