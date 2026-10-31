#![cfg(feature = "test-support")]

#[test]
fn fake_herdr_is_available_to_integration_tests() {
    use herdr_shepherd::herdr::{Herdr, fake::FakeHerdr};
    let h = FakeHerdr::new(vec![]);
    assert!(h.workspaces().unwrap().is_empty());
    assert_eq!(*h.calls.borrow(), ["workspaces"]);
}
