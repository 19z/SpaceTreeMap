fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg-bin=space_tree_map=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=space_tree_map=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
    }
}
