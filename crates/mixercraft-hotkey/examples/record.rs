//! Records one key combination and prints it (try the recorder by hand):
//! `cargo run -p mixercraft-hotkey --example record`.
fn main() {
    println!("press a key combination (Escape cancels, 10 s timeout)");
    match mixercraft_hotkey::capture(std::time::Duration::from_secs(10)) {
        Ok(Some(c)) => println!("{c} ({})", if c.check_bindable().is_ok() { "bindable" } else { "needs a modifier" }),
        Ok(None) => println!("nothing recorded"),
        Err(e) => println!("error: {e}"),
    }
}
