use agent_client_protocol as acp;

use fluent_code_tui::AcpClientRuntime;

#[test]
fn direct_impl_public_runtime_satisfies_client_trait() {
    fn assert_client<T: acp::Client>() {}

    assert_client::<AcpClientRuntime>();
}
