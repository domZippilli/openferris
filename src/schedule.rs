use anyhow::Result;

/// Marker comment so we can identify our entries in the crontab.
const CRON_MARKER: &str = "# openferris:";

fn read_crontab() -> Result<String> {
    let output = std::process::Command::new("crontab")
        .arg("-l")
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run crontab: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        // Empty crontab returns error on some systems
        Ok(String::new())
    }
}

fn write_crontab(content: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to run crontab: {}", e))?;

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(content.as_bytes())?;

    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("crontab returned error");
    }
    Ok(())
}

/// Find the openferris binary path for cron entries.
fn binary_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "openferris".to_string())
}

pub fn add(skill_name: &str, cron_expr: &str) -> Result<String> {
    let mut crontab = read_crontab()?;
    let marker = format!("{} {}", CRON_MARKER, skill_name);

    // Check if already scheduled
    if crontab.lines().any(|l| l.contains(&marker)) {
        anyhow::bail!(
            "Skill '{}' is already scheduled. Remove it first.",
            skill_name
        );
    }

    let entry = format!(
        "{} {} run {} {}\n",
        cron_expr,
        binary_path(),
        skill_name,
        marker
    );

    crontab.push_str(&entry);
    write_crontab(&crontab)?;

    Ok(format!("Scheduled '{}': {}", skill_name, cron_expr))
}

pub fn remove(skill_name: &str) -> Result<String> {
    let crontab = read_crontab()?;
    let marker = format!("{} {}", CRON_MARKER, skill_name);

    let new_crontab: String = crontab
        .lines()
        .filter(|l| !l.contains(&marker))
        .collect::<Vec<_>>()
        .join("\n");

    let removed = crontab.lines().count() != new_crontab.lines().count();

    if !removed {
        return Ok(format!("No schedule found for '{}'.", skill_name));
    }

    // Ensure trailing newline
    let new_crontab = if new_crontab.is_empty() {
        String::new()
    } else {
        format!("{}\n", new_crontab.trim_end())
    };

    write_crontab(&new_crontab)?;
    Ok(format!("Removed schedule for '{}'.", skill_name))
}

pub fn list() -> Result<String> {
    let crontab = read_crontab()?;
    let entries: Vec<&str> = crontab
        .lines()
        .filter(|l| l.contains(CRON_MARKER))
        .collect();

    if entries.is_empty() {
        return Ok("No scheduled skills.".to_string());
    }

    let mut output = String::from("Scheduled skills:\n");
    for entry in entries {
        if let Some(marker_pos) = entry.find(CRON_MARKER) {
            let skill = entry[marker_pos + CRON_MARKER.len()..].trim();
            let cron_part: String = entry[..marker_pos]
                .split_whitespace()
                .take(5)
                .collect::<Vec<_>>()
                .join(" ");
            output.push_str(&format!("  {} — {}\n", skill, cron_part));
        }
    }
    Ok(output)
}
