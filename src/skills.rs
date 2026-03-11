use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub prompt: String,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    tools: Vec<String>,
}

pub fn load_skill_from_str(content: &str) -> Result<Skill> {
    let (frontmatter, body) = parse_frontmatter(content)?;
    let meta: SkillFrontmatter =
        serde_yaml::from_str(&frontmatter).context("Failed to parse skill frontmatter")?;

    Ok(Skill {
        name: meta.name,
        description: meta.description,
        tools: meta.tools,
        prompt: body.trim().to_string(),
    })
}

pub fn load_skill_from_file(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read skill: {}", path.display()))?;
    load_skill_from_str(&content)
}

fn parse_frontmatter(content: &str) -> Result<(String, String)> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        anyhow::bail!("Skill file must start with '---' frontmatter delimiter");
    }

    let after_first = &content[3..];
    let second_delimiter = after_first
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("Missing closing '---' frontmatter delimiter"))?;

    let frontmatter = after_first[..second_delimiter].trim().to_string();
    let body = after_first[second_delimiter + 4..].to_string();

    Ok((frontmatter, body))
}

/// Bundled skills compiled into the binary
fn get_bundled_skill(name: &str) -> Option<&'static str> {
    match name {
        "default" => Some(include_str!("../skills/default/SKILL.md")),
        "daily-briefing" => Some(include_str!("../skills/daily-briefing/SKILL.md")),
        "email-reply" => Some(include_str!("../skills/email-reply/SKILL.md")),
        _ => None,
    }
}

/// Load a skill by name. Lookup order:
/// 1. User skills: ~/.config/openferris/skills/<name>/SKILL.md
/// 2. Workspace skills: ~/.local/share/openferris/workspace/skills/<name>/SKILL.md (agent-created)
/// 3. Bundled skills compiled into the binary
pub fn load_skill(name: &str, user_skills_dir: &Path) -> Result<Skill> {
    // 1. User-managed skills
    let user_skill_path = user_skills_dir.join(name).join("SKILL.md");
    if user_skill_path.exists() {
        return load_skill_from_file(&user_skill_path);
    }

    // 2. Agent-created skills in workspace
    let workspace_skill_path = crate::config::data_dir()
        .join("workspace")
        .join("skills")
        .join(name)
        .join("SKILL.md");
    if workspace_skill_path.exists() {
        return load_skill_from_file(&workspace_skill_path);
    }

    // 3. Bundled skills
    if let Some(content) = get_bundled_skill(name) {
        return load_skill_from_str(content);
    }

    anyhow::bail!(
        "Skill '{}' not found. Checked:\n  - {}\n  - {}\n  - bundled skills",
        name,
        user_skill_path.display(),
        workspace_skill_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter() {
        let content = "---\nname: test\ndescription: A test skill\ntools:\n  - datetime\n---\n\nDo the thing.\n";
        let skill = load_skill_from_str(content).unwrap();
        assert_eq!(skill.name, "test");
        assert_eq!(skill.description, "A test skill");
        assert_eq!(skill.tools, vec!["datetime"]);
        assert_eq!(skill.prompt, "Do the thing.");
    }
}
