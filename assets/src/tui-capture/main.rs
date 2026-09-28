// Drives the real TUI render code (ui::draw) with the real mock-data source
// (MockMetrics, the same one `tui` uses without --live) on an off-screen
// TestBackend, one frame per data tick, and dumps every cell as JSON lines.
use ratatui::{backend::TestBackend, Terminal};
use std::io::Write;
use std::time::Duration;
use tokio_prompt_orchestrator::tui::{app::App, metrics::MockMetrics, ui};

fn esc(s: &str) -> String {
    let mut o = String::new();
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (w, h): (u16, u16) = (args[1].parse().unwrap(), args[2].parse().unwrap());
    let from: u64 = args[3].parse().unwrap();
    let to: u64 = args[4].parse().unwrap();
    let sleep_ms: u64 = args[5].parse().unwrap();
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    let mut app = App::new(Duration::from_millis(1000));
    let mock = MockMetrics::new();
    let out = std::io::stdout();
    let mut out = out.lock();
    for t in 1..=to {
        mock.tick(&mut app);
        if t < from {
            continue;
        }
        std::thread::sleep(Duration::from_millis(sleep_ms));
        term.draw(|f| ui::draw(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let mut cells = Vec::new();
        for c in buf.content() {
            cells.push(format!(
                "[\"{}\",\"{:?}\",\"{:?}\",{}]",
                esc(c.symbol()),
                c.fg,
                c.bg,
                c.modifier.bits()
            ));
        }
        writeln!(
            out,
            "{{\"tick\":{},\"w\":{},\"h\":{},\"cells\":[{}]}}",
            t,
            w,
            h,
            cells.join(",")
        )
        .unwrap();
    }
}
