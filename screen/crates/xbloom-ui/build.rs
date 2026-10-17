//! Compile the Slint UI.
//!
//! Hosted builds load fonts and images from embedded files at runtime. MCU builds (the
//! ESP32-S3) pre-render everything for the software renderer at compile time; fonts become
//! signed-distance fields — one glyph set per weight that scales to every size — instead of a
//! bitmap per size, which keeps five weights of Inter in about a megabyte of flash.
fn main() {
    let mcu = std::env::var_os("CARGO_FEATURE_MCU").is_some();
    let config = if mcu {
        // Slint renders SDF glyphs at 2/3 of the largest listed size (capped at 6× the
        // smallest). Font sizes come from the Theme global, which the compiler doesn't count,
        // so list them: 12…84 px gives 56 px glyphs — crisp from captions up to the 120 px
        // clock, at ~200 KB per weight.
        std::env::set_var("SLINT_FONT_SIZES", "12,84");
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer)
            .with_sdf_fonts(true)
    } else {
        slint_build::CompilerConfiguration::new().embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
    };
    slint_build::compile_with_config("ui/app.slint", config).expect("compile the Slint UI");
}
