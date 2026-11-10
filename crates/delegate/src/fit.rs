//! Fixed-size, user-local model suitability card; never a dispatch gate.
use crate::common::*;
use serde_json::{json, Value};

pub const CLASSES: [&str; 4] = ["scout", "build", "hard", "design"];
fn path() -> std::path::PathBuf {
    crate::config::user_path().with_file_name("fit.json")
}
pub fn validate_class(class: &str) -> Res<()> {
    if CLASSES.contains(&class) {
        Ok(())
    } else {
        Err(format!("unknown class {class}; use {}", CLASSES.join("|")))
    }
}
fn load() -> Res<Value> {
    Ok(crate::config::read_file(&path())?.unwrap_or(json!({})))
}
fn aliases(config: &Value) -> Vec<String> {
    config["models"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k.clone())
        .collect()
}
fn unreviewed(card: &Value, class: &str, aliases: &[String]) -> Vec<String> {
    aliases
        .iter()
        .filter(|a| card[class][a.as_str()].is_null())
        .cloned()
        .collect()
}
fn set(
    card: &mut Value,
    class: &str,
    alias: &str,
    verdict: &str,
    note: &str,
    sample: Option<&str>,
) -> Res<Option<String>> {
    validate_class(class)?;
    if !["first", "ok", "avoid"].contains(&verdict) {
        return Err("verdict must be first|ok|avoid".into());
    }
    if note.chars().count() > 80 {
        return Err("note 上限 80 个字符；请压成一句话后重试".into());
    }
    if note.contains(['\n', '\r']) {
        return Err("note 必须是一句话；去掉换行后重试".into());
    }
    let mut demoted = None;
    if verdict == "first" {
        if let Some(entries) = card[class].as_object_mut() {
            for (name, entry) in entries {
                if name != alias && s(entry, "verdict") == "first" {
                    entry["verdict"] = json!("ok");
                    demoted = Some(name.clone());
                }
            }
        }
    }
    card[class][alias] =
        json!({"verdict":verdict,"note":note,"date":iso().split('T').next().unwrap_or_default()});
    if let Some(sample) = sample {
        card[class][alias]["sample"] = json!(sample);
    }
    Ok(demoted)
}

pub fn notices(class: &str, alias: &str, config: &Value) -> Res<Vec<String>> {
    let card = load()?;
    let alias = crate::models::selected_alias(alias, config);
    let aliases = aliases(config);
    let mut out = vec![];
    if s(&card[class][alias], "verdict") == "avoid" {
        out.push(format!(
            "{alias} 在 {class} 类别标记 avoid：{}",
            s(&card[class][alias], "note")
        ));
    }
    if let Some(first) = aliases
        .iter()
        .find(|a| s(&card[class][a.as_str()], "verdict") == "first")
    {
        if first != alias {
            out.push(format!("该类首选是 {first}"));
        }
    }
    if !unreviewed(&card, class, &aliases).is_empty() {
        out.push("首次遇到这类任务可各派一路对比：delegate fit".into());
    }
    Ok(out)
}

pub fn command(args: &[String]) -> Res<i32> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let config = crate::config::load(git_top(&cwd).as_deref())?;
    let aliases = aliases(&config.value);
    if args.first().is_some_and(|a| a == "set") {
        if args.len() != 5 && !(args.len() == 7 && args[5] == "--sample") {
            return Err("use delegate fit set <scout|build|hard|design> <alias> <first|ok|avoid> \"note\" [--sample FILE]".into());
        }
        if !aliases.contains(&args[2]) {
            return Err(format!("unknown alias {}; see delegate tiers", args[2]));
        }
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let _guard = lock(&path.with_extension("lock"), true, false).map_err(|e| e.to_string())?;
        let mut card = load()?;
        let demoted = set(
            &mut card,
            &args[1],
            &args[2],
            &args[3],
            &args[4],
            args.get(6).map(String::as_str),
        )?;
        write_json(&path, &card)?;
        if let Some(alias) = demoted {
            println!("{alias} 自动降为 ok");
        }
        println!("已记录 {} × {}；查看：delegate fit", args[1], args[2]);
    } else {
        if !args.is_empty() && args != ["--json"] {
            return Err("use delegate fit [--json] or delegate fit set …".into());
        }
        let card = load()?;
        if args == ["--json"] {
            println!("{card}");
            return Ok(0);
        }
        // Four rows only; compact each cell for wide model registries. Full notes
        // remain available through --json, without growing the summary vertically.
        let width = (100 / aliases.len().max(1)).clamp(10, 85);
        println!("类别 | {}", aliases.join(" | "));
        for class in CLASSES {
            let cells = aliases
                .iter()
                .map(|a| {
                    let entry = &card[class][a.as_str()];
                    if entry.is_null() {
                        return "—".into();
                    }
                    let text = format!("{} · {}", s(entry, "verdict"), s(entry, "note"));
                    if text.chars().count() > width {
                        format!("{}…", text.chars().take(width - 1).collect::<String>())
                    } else {
                        text
                    }
                })
                .collect::<Vec<_>>();
            println!("{class} | {}", cells.join(" | "));
        }
        let missing = CLASSES
            .iter()
            .flat_map(|class| {
                unreviewed(&card, class, &aliases)
                    .into_iter()
                    .map(move |a| format!("{class} × {a}"))
            })
            .collect::<Vec<_>>();
        println!(
            "还没评过的（类别 × 别名）：{}",
            if missing.is_empty() {
                "无".into()
            } else {
                missing.join("，")
            }
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn note_limit() {
        assert!(
            set(&mut json!({}), "build", "a", "ok", &"字".repeat(81), None)
                .unwrap_err()
                .contains("80")
        );
    }
    #[test]
    fn overwrites() {
        let mut card = json!({});
        set(&mut card, "build", "a", "ok", "old", Some("spec")).unwrap();
        set(&mut card, "build", "a", "avoid", "new", None).unwrap();
        assert_eq!(s(&card["build"]["a"], "note"), "new");
        assert!(card["build"]["a"]["sample"].is_null());
    }
    #[test]
    fn first_demotes() {
        let mut card = json!({});
        set(&mut card, "hard", "a", "first", "a", None).unwrap();
        assert_eq!(
            set(&mut card, "hard", "b", "first", "b", None)
                .unwrap()
                .as_deref(),
            Some("a")
        );
        assert_eq!(s(&card["hard"]["a"], "verdict"), "ok");
    }
}
