mitb_sdk::policy_prelude!("always-wait");

#[derive(Default)]
struct AlwaysWait;

impl Policy for AlwaysWait {
    async fn act(&mut self, _contents: String) -> ActionResult {
        Ok(Action::Wait)
    }
}

bindings::export_policy!(AlwaysWait);
