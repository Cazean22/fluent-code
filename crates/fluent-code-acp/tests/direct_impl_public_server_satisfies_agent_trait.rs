use agent_client_protocol as acp;

use fluent_code_acp::AcpServer;

#[test]
fn direct_impl_public_server_satisfies_agent_trait() {
    fn assert_agent<T: acp::Agent>() {}

    assert_agent::<AcpServer>();
}
