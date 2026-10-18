//! Hand the ESP-IDF build environment (linker args, cfgs) to cargo. The UI itself is compiled
//! by `xbloom-ui` (its `mcu` feature pre-renders fonts and images for the software renderer).
fn main() {
    embuild::espidf::sysenv::output();
}
