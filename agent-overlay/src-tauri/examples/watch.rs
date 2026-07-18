//! Poll discovery a few times to exercise the streak-based status logic.
fn main() {
    for round in 0..4 {
        let sessions = agent_overlay_lib::discover_sessions();
        println!("--- poll {round}");
        for s in &sessions {
            println!("{:12} {:7} {}", s.pane_id, s.status, s.cwd);
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
