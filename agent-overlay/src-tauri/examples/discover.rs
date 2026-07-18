fn main() {
    println!("{}", serde_json::to_string_pretty(&agent_overlay_lib::discover_sessions()).unwrap());
}
