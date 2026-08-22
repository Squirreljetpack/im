use std::io::Write;

/// Dummy notification mechanism: prints terminal bell (\x07) and "title: message" to stdout.
pub fn notify(title: &str, message: &str) {
    println!("\x07{}: {}", title, message);
    let _ = std::io::stdout().flush();
}
