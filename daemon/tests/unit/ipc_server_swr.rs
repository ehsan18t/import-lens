use super::SwrRefreshLifecycle;
use std::sync::atomic::Ordering;

#[test]
fn swr_lifecycle_cancels_only_the_superseded_document() {
    let mut lifecycle = SwrRefreshLifecycle::new();

    let first = lifecycle.start_document("C:/workspace", "C:/workspace/src/a.ts");
    let other = lifecycle.start_document("C:/workspace", "C:/workspace/src/b.ts");
    assert!(
        !first.load(Ordering::Acquire),
        "starting another document must not cancel this document's SWR"
    );
    assert!(
        !other.load(Ordering::Acquire),
        "the unrelated document starts live"
    );

    let replacement = lifecycle.start_document("C:/workspace", "C:/workspace/src/a.ts");
    assert!(
        first.load(Ordering::Acquire),
        "a newer request for the same document cancels the older SWR"
    );
    assert!(
        !other.load(Ordering::Acquire),
        "same-document cancellation must not affect a different document"
    );
    assert!(
        !replacement.load(Ordering::Acquire),
        "the replacement request starts live"
    );

    drop(lifecycle);
    assert!(
        replacement.load(Ordering::Acquire) && other.load(Ordering::Acquire),
        "ending the connection cancels every active SWR flag"
    );
}
