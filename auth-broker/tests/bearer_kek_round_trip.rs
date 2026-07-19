use botwork_auth_broker::auth::{unwrap_session_key, wrap_session_key, Bearer, LeaseKekError};

#[test]
fn bearer_kek_round_trip() {
    let bearer = Bearer::generate();
    let session_key = b"known-session-key-material";
    let wrapped = wrap_session_key(bearer.as_bytes(), session_key);
    let unwrapped = unwrap_session_key(bearer.as_bytes(), &wrapped).expect("round trip");
    assert_eq!(unwrapped.as_slice(), session_key);
}

#[test]
fn bearer_kek_wrong_bearer_fails() {
    let bearer = Bearer::generate();
    let other = Bearer::generate();
    let wrapped = wrap_session_key(bearer.as_bytes(), b"known-session-key-material");
    let err = unwrap_session_key(other.as_bytes(), &wrapped).expect_err("wrong bearer must fail");
    assert!(matches!(err, LeaseKekError::Decrypt), "got {err:?}");
}
