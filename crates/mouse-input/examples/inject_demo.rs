//! Runtime smoke test for the Linux injector against the live X server.
//! Run with: `cargo run -p mouse-input --example inject_demo`
//!
//! Moves the real cursor and reads it back to confirm XTEST injection works.

use mouse_input::new_injector;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut inj = new_injector()?;

    let start = inj.cursor_pos()?;
    println!("cursor start: {start:?}");

    // Absolute move to a known point.
    inj.mouse_move_abs(400, 300)?;
    std::thread::sleep(std::time::Duration::from_millis(50));
    let at = inj.cursor_pos()?;
    println!("after move_abs(400,300): {at:?}");
    assert!(
        (at.0 - 400).abs() <= 1 && (at.1 - 300).abs() <= 1,
        "absolute move failed"
    );

    // Relative move by (+120, +80).
    inj.mouse_move_rel(120, 80)?;
    std::thread::sleep(std::time::Duration::from_millis(50));
    let rel = inj.cursor_pos()?;
    println!("after move_rel(+120,+80): {rel:?}");
    assert!(
        (rel.0 - 520).abs() <= 1 && (rel.1 - 380).abs() <= 1,
        "relative move failed"
    );

    // Restore the cursor roughly where it started.
    inj.mouse_move_abs(start.0, start.1)?;

    println!("OK: XTEST injection + cursor_pos verified on the live X server");
    Ok(())
}
