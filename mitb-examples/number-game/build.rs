fn main() -> Result<(), mitb_sdk::build_support::BuildSupportError> {
    mitb_sdk::build_support::write_policy_bindgen()?;
    mitb_sdk::build_support::print_rerun_if_mitb_wit_changed();
    Ok(())
}
