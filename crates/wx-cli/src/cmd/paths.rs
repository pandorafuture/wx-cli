use std::path::Path;

use wx_paths::PathsSummary;

pub fn cmd_paths(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let ap = wx_paths::AppPaths::new()?;
    let summary = ap.summary();
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_paths_table(&summary);
    }
    Ok(())
}

fn print_paths_table(summary: &PathsSummary) {
    println!("Platform: {}", summary.platform);
    println!();

    let rows: Vec<(&str, &Path)> = vec![
        ("config_dir", &summary.config_dir),
        ("keys_file", &summary.keys_file),
        ("settings_file", &summary.settings_file),
        ("cache_root", &summary.cache_root),
        ("state_root", &summary.state_root),
        ("logs_dir", &summary.logs_dir),
        ("server_state_dir", &summary.server_state_dir),
        ("server_stdout_log", &summary.server_stdout_log),
        ("server_stderr_log", &summary.server_stderr_log),
        ("temp_root", &summary.temp_root),
    ];

    let max_label = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0);

    for (label, path) in &rows {
        let display = tilde_path(path);
        let status = if path.exists() {
            "[exists]"
        } else {
            "[missing]"
        };
        println!(
            "{:<width$}  {:<60}  {}",
            label,
            display,
            status,
            width = max_label
        );
    }
}

fn tilde_path(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let home_path = Path::new(&home);
        if let Ok(suffix) = path.strip_prefix(home_path) {
            return format!("~/{}", suffix.display());
        }
    }
    path.display().to_string()
}
