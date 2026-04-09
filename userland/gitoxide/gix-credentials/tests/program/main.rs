use gix_credentials::program::main;
use std::io::Cursor;

#[derive(Debug, thiserror::Error)]
#[error("Test error")]
struct TestError;

#[test]
fn protocol_and_host_without_url_is_valid() {
    let input = b"protocol=https\nhost=github.com\n";
    let mut output = Vec::new();

    let mut called = false;
    let result = main(
        ["get".into()],
        Cursor::new(input),
        &mut output,
        |_action, context| -> Result<Option<gix_credentials::protocol::Context>, TestError> {
            assert_eq!(context.protocol.as_deref(), Some("https"));
            assert_eq!(context.host.as_deref(), Some("github.com"));
            assert_eq!(context.url, None, "the URL isn't automatically populated");
            called = true;

            Ok(None)
        },
    );

    // This should fail because our mock helper returned None (no credentials found)
    // but it should NOT fail because of missing URL
    match result {
        Err(gix_credentials::program::main::Error::CredentialsMissing { .. }) => {
            assert!(
                called,
                "The helper gets called, but as nothing is provided in the function it ulimately fails"
            );
        }
        other => panic!("Expected CredentialsMissing error, got: {other:?}"),
    }
}

#[test]
fn missing_protocol_with_only_host_or_protocol_fails() {
    for input in ["host=github.com\n", "protocol=https\n"] {
        let mut output = Vec::new();

        let mut called = false;
        let result = main(
            ["get".into()],
            Cursor::new(input),
            &mut output,
            |_action, _context| -> Result<Option<gix_credentials::protocol::Context>, TestError> {
                called = true;
                Ok(None)
            },
        );

        match result {
            Err(gix_credentials::program::main::Error::UrlMissing) => {
                assert!(!called, "the context is lacking, hence nothing gets called");
            }
            other => panic!("Expected UrlMissing error, got: {other:?}"),
        }
    }
}

#[test]
fn url_alone_is_valid() {
    let input = b"url=https://github.com\n";
    let mut output = Vec::new();

    let mut called = false;
    let result = main(
        ["get".into()],
        Cursor::new(input),
        &mut output,
        |_action, context| -> Result<Option<gix_credentials::protocol::Context>, TestError> {
            called = true;
            assert_eq!(context.url.unwrap(), "https://github.com");
            assert_eq!(context.host, None, "not auto-populated");
            assert_eq!(context.protocol, None, "not auto-populated");

            Ok(None)
        },
    );

    // This should fail because our mock helper returned None (no credentials found)
    // but it should NOT fail because of missing URL
    match result {
        Err(gix_credentials::program::main::Error::CredentialsMissing { .. }) => {
            assert!(called);
        }
        other => panic!("Expected CredentialsMissing error, got: {other:?}"),
    }
}
