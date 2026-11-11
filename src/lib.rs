#[cfg(windows)]
pub mod app;
#[cfg(not(windows))]
#[path = "app_stub.rs"]
pub mod app;
#[cfg(windows)]
pub mod model;
#[cfg(windows)]
pub mod scanner;
#[cfg(windows)]
pub mod shell;
#[cfg(windows)]
pub mod treemap;

pub fn format_size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", units[unit])
    }
}
