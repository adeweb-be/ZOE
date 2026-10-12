//! select! polled by a plain executor, outside any tokio runtime.
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

#[test]
fn select_outside_a_runtime() {
    let picks: Vec<u8> = block_on(async {
        let mut out = Vec::new();
        for _ in 0..32 {
            let v = tokio::select! { v = std::future::ready(0u8) => v, v = std::future::ready(1u8) => v };
            out.push(v);
        }
        out
    });
    let biased: Vec<u8> = block_on(async {
        let mut out = Vec::new();
        for _ in 0..32 {
            let v = tokio::select! { biased; v = std::future::ready(0u8) => v, v = std::future::ready(1u8) => v };
            out.push(v);
        }
        out
    });
    println!("PICKS {picks:?}");
    println!("BIASED {biased:?}");
}
