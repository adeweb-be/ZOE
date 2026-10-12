#[test]
fn builds() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async { tokio::task::yield_now().await });
}
