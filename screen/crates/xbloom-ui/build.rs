//! Compile the Slint UI. MCU builds pre-render fonts/images for the software renderer.
fn main() {
    let kind = if std::env::var_os("CARGO_FEATURE_MCU").is_some() {
        slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer
    } else {
        slint_build::EmbedResourcesKind::EmbedFiles
    };
    let config = slint_build::CompilerConfiguration::new().embed_resources(kind);
    slint_build::compile_with_config("ui/app.slint", config).expect("compile the Slint UI");
}
