use envoy_proto::envoy::service::ext_proc::v3::ProcessingRequest;

#[test]
fn processing_request_is_a_prost_message() {
    fn assert_message<T: prost::Message>() {}

    assert_message::<ProcessingRequest>();
}
