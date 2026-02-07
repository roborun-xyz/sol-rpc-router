use sol_rpc_router::mock::MockKeyStore;
use sol_rpc_router::keystore::KeyStore;

#[tokio::test]
async fn test_validate_key_valid() {
    let store = MockKeyStore::new();
    store.add_key("valid-key", "owner1", 100);

    let result = store.validate_key("valid-key").await;
    assert!(result.is_ok());
    let info = result.unwrap().unwrap();
    assert_eq!(info.owner, "owner1");
    assert_eq!(info.rate_limit, 100);
}

#[tokio::test]
async fn test_validate_key_invalid() {
    let store = MockKeyStore::new();
    let result = store.validate_key("invalid-key").await;
    assert!(result.is_ok()); // Returns Ok(None)
    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn test_validate_key_inactive() {
    let store = MockKeyStore::new();
    store.add_key("inactive-key", "owner2", 100);
    store.set_inactive("inactive-key");

    let result = store.validate_key("inactive-key").await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn test_validate_key_rate_limit() {
    let store = MockKeyStore::new();
    store.add_key("limited-key", "owner3", 10);
    
    // Simulate rate limit hit by manually adding to restricted list in MockKeyStore
    store.rate_limited_keys.lock().unwrap().push("limited-key".to_string());

    let result = store.validate_key("limited-key").await;
    assert!(result.is_err());
    assert_eq!(result.err().unwrap(), "Rate limit exceeded");
}
