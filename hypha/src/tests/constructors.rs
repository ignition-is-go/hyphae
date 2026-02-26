use std::time::Duration;

use crate::from_iter_with_delay;

#[test]
fn test_from_iter_with_delay_empty() {
    let items: Option<_> = from_iter_with_delay(Vec::<u64>::new(), Duration::from_millis(30));
    assert!(items.is_none());
}
