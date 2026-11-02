//! The base of the size measure: the scratch delta store alone.

fn main() {
    let path = std::env::args().nth(1).unwrap_or_default();
    let store = zoe_core::text::TextDeltas::create(path.as_ref(), zoe_core::Compression::None);
    println!("{}", store.is_ok());
}
