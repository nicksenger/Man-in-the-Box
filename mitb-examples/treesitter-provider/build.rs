fn main() -> Result<(), mitb_sdk::build_support::BuildSupportError> {
    mitb_sdk::build_support::write_bindgen_for_world_named(
        "mitb:treesitter/provider",
        "treesitter_bindgen.rs",
    )?;
    mitb_sdk::build_support::print_rerun_if_mitb_wit_changed();
    Ok(())
}
