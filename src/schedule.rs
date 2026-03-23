use anyhow::Result;

/// Marker comment so we can identify our entries in the crontab.
const CRON_MARKER: &str = "# openferris:";

/// Validate that a skill name is safe to embed in a crontab entry.
///
/// Must start with an alphanumeric character and contain only alphanumerics,
/// hyphens, or underscores.
fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("Skill name must not be empty");
    }

    let valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');

    if !valid {
        anyhow::bail!(
            "Invalid skill name '{}': must start with an alphanumeric character \
             and contain only alphanumerics, hyphens, or underscores",
            name
        );
    }
    Ok(())
}

/// Validate that a cron expression is safe and well-formed.
///
/// Must have exactly 5 whitespace-separated fields (minute, hour, day-of-month,
/// month, day-of-week). Each field may only contain digits, `*`, `/`, `,`, `-`.
/// The expression must not contain shell metacharacters.
fn validate_cron_expr(expr: &str) -> Result<()> {
    // Reject dangerous characters that could allow command injection.
    const FORBIDDEN: &[char] = &['\n', '\r', ';', '`', '|', '$', '&', '(', ')', '{', '}', '<', '>', '\'', '"', '\\', '!', '#'];
    if let Some(bad) = expr.chars().find(|c| FORBIDDEN.contains(c)) {
        anyhow::bail!(
            "Invalid cron expression: contains forbidden character '{}'",
            bad
        );
    }

    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        anyhow::bail!(
            "Invalid cron expression '{}': expected 5 fields, got {}",
            expr,
            fields.len()
        );
    }

    for (i, field) in fields.iter().enumerate() {
        if !field.chars().all(|c| c.is_ascii_digit() || matches!(c, '*' | '/' | ',' | '-')) {
            anyhow::bail!(
                "Invalid cron expression: field {} ('{}') contains invalid characters \
                 (only digits, *, /, comma, - are allowed)",
                i + 1,
                field
            );
        }
    }

    Ok(())
}

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
        .ok_or_else(|| anyhow::anyhow!("Failed to open stdin pipe for crontab"))?
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
    validate_skill_name(skill_name)?;
    validate_cron_expr(cron_expr)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_skill_name ──────────────────────────────────────────

    #[test]
    fn valid_skill_names() {
        assert!(validate_skill_name("daily-briefing").is_ok());
        assert!(validate_skill_name("backup2").is_ok());
        assert!(validate_skill_name("a").is_ok());
        assert!(validate_skill_name("My_Skill_01").is_ok());
        assert!(validate_skill_name("X").is_ok());
        assert!(validate_skill_name("task-1_2").is_ok());
    }

    #[test]
    fn empty_skill_name_rejected() {
        let err = validate_skill_name("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn skill_name_starting_with_hyphen_rejected() {
        let err = validate_skill_name("-bad").unwrap_err();
        assert!(err.to_string().contains("must start with an alphanumeric"));
    }

    #[test]
    fn skill_name_starting_with_underscore_rejected() {
        let err = validate_skill_name("_bad").unwrap_err();
        assert!(err.to_string().contains("must start with an alphanumeric"));
    }

    #[test]
    fn skill_name_with_spaces_rejected() {
        let err = validate_skill_name("my skill").unwrap_err();
        assert!(err.to_string().contains("Invalid skill name"));
    }

    #[test]
    fn skill_name_with_semicolon_rejected() {
        let err = validate_skill_name("ok;rm -rf /").unwrap_err();
        assert!(err.to_string().contains("Invalid skill name"));
    }

    #[test]
    fn skill_name_with_backtick_rejected() {
        let err = validate_skill_name("skill`whoami`").unwrap_err();
        assert!(err.to_string().contains("Invalid skill name"));
    }

    #[test]
    fn skill_name_with_dollar_rejected() {
        let err = validate_skill_name("skill$(id)").unwrap_err();
        assert!(err.to_string().contains("Invalid skill name"));
    }

    #[test]
    fn skill_name_with_newline_rejected() {
        let err = validate_skill_name("skill\nmalicious").unwrap_err();
        assert!(err.to_string().contains("Invalid skill name"));
    }

    // ── validate_cron_expr ───────────────────────────────────────────

    #[test]
    fn valid_cron_expressions() {
        assert!(validate_cron_expr("0 9 * * *").is_ok());
        assert!(validate_cron_expr("*/15 * * * *").is_ok());
        assert!(validate_cron_expr("0 0 1,15 * *").is_ok());
        assert!(validate_cron_expr("30 6 * * 1-5").is_ok());
        assert!(validate_cron_expr("0 */2 * * *").is_ok());
    }

    #[test]
    fn cron_expr_wrong_field_count_rejected() {
        let err = validate_cron_expr("0 9 * *").unwrap_err();
        assert!(err.to_string().contains("expected 5 fields, got 4"));

        let err = validate_cron_expr("0 9 * * * *").unwrap_err();
        assert!(err.to_string().contains("expected 5 fields, got 6"));
    }

    #[test]
    fn cron_expr_with_newline_rejected() {
        let err = validate_cron_expr("0 9 * * *\n0 * * * * rm -rf /").unwrap_err();
        assert!(err.to_string().contains("forbidden character"));
    }

    #[test]
    fn cron_expr_with_semicolon_rejected() {
        let err = validate_cron_expr("0 9 * * *; rm -rf /").unwrap_err();
        assert!(err.to_string().contains("forbidden character"));
    }

    #[test]
    fn cron_expr_with_backtick_rejected() {
        let err = validate_cron_expr("`whoami` 9 * * *").unwrap_err();
        assert!(err.to_string().contains("forbidden character"));
    }

    #[test]
    fn cron_expr_with_pipe_rejected() {
        let err = validate_cron_expr("0 9 * * * | mail").unwrap_err();
        assert!(err.to_string().contains("forbidden character"));
    }

    #[test]
    fn cron_expr_with_dollar_rejected() {
        let err = validate_cron_expr("0 9 * * $HOME").unwrap_err();
        assert!(err.to_string().contains("forbidden character"));
    }

    #[test]
    fn cron_expr_with_invalid_field_chars_rejected() {
        let err = validate_cron_expr("0 9 abc * *").unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn cron_expr_empty_rejected() {
        let err = validate_cron_expr("").unwrap_err();
        assert!(err.to_string().contains("expected 5 fields, got 0"));
    }
}
