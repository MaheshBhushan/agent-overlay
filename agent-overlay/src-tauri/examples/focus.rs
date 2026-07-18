fn main() {
    let handle = std::env::args().nth(1).expect("usage: focus <handle>");
    match agent_overlay_lib::focus_handle(&handle) {
        Ok(()) => println!("focused ok"),
        Err(e) => println!("focus failed: {e}"),
    }
}
