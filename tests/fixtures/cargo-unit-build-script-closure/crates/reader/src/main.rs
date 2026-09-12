include!(concat!(env!("OUT_DIR"), "/message.rs"));

fn main() {
    assert_eq!(env!("CARGO_UNIT_COMPILE_ENV"), "scoped compile input");
    assert_eq!(MESSAGE, "declared sibling input survives build-script staging\n");
    print!("{MESSAGE}");
}
