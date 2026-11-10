//! Independent purpose, model identity and thinking selection.
use crate::agents::{self, Clock};
use crate::common::*;
use serde_json::{json, Value};

pub const TIERS: [&str; 3] = ["scout", "review", "write"];
pub const THINKING: [&str; 6] = ["off", "minimal", "low", "medium", "high", "xhigh"];

#[derive(Debug, Clone)]
pub struct PiModel {
    pub name: String,
    pub context: String,
    pub thinking: bool,
    pub images: bool,
}

pub fn parse_pi_models(table: &str) -> Vec<PiModel> {
    table
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("provider"))
        .skip(1)
        .filter_map(|line| {
            let c: Vec<_> = line.split_whitespace().collect();
            (c.len() >= 6 && ["yes", "no"].contains(&c[5])).then(|| PiModel {
                name: format!("{}/{}", c[0], c[1]),
                context: c[2].into(),
                thinking: c[4] == "yes",
                images: c[5] == "yes",
            })
        })
        .collect()
}

pub fn validate_model(model: &str) -> Res<()> {
    if model
        .rsplit_once(':')
        .is_some_and(|(_, suffix)| THINKING.contains(&suffix) || suffix == "max")
    {
        return Err(format!("{model}: model names cannot contain :thinking; use the thinking field (CLI: --thinking)"));
    }
    if model.is_empty() || model.contains(['\0', ' ', '\n']) {
        return Err("model must be a non-empty alias or provider/id".into());
    }
    Ok(())
}
pub fn validate_thinking(level: &str) -> Res<()> {
    if THINKING.contains(&level) {
        Ok(())
    } else {
        Err(format!(
            "invalid thinking {level}: use {}",
            THINKING.join("|")
        ))
    }
}

/// The configured alias table is the whole selectable universe. Pi contributes
/// validation and capability metadata only, never unregistered candidates.
pub fn alias_model(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| value["model"].as_str())
}

/// Preserve exact aliases (including distinct defaults for the same model),
/// otherwise map the resolved identity back to a registered alias.
pub fn selected_alias<'a>(input: &'a str, config: &'a Value) -> &'a str {
    let Some(aliases) = config["models"].as_object() else {
        return input;
    };
    if aliases.contains_key(input) {
        return input;
    }
    if let Some((name, _)) = aliases
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(input))
    {
        return name;
    }
    let Ok(id) = resolve_model(input, config, &[]) else {
        return input;
    };
    aliases
        .iter()
        .find(|(_, v)| alias_model(v) == Some(id.as_str()))
        .map_or(input, |(name, _)| name)
}

pub fn resolve_model(input: &str, config: &Value, listed: &[PiModel]) -> Res<String> {
    validate_model(input)?;
    let aliases = config["models"].as_object();
    if let Some(id) = aliases.and_then(|a| a.get(input)).and_then(alias_model) {
        return check_full(id, listed);
    }
    let entries = aliases
        .into_iter()
        .flatten()
        .filter_map(|(name, id)| alias_model(id).map(|id| (name.as_str(), id)))
        .collect::<Vec<_>>();
    if entries.iter().any(|(_, id)| *id == input) {
        return check_full(input, listed);
    }
    let needle = input.to_lowercase();
    let mut matches = entries
        .iter()
        .filter(|(alias, id)| {
            alias.to_lowercase().contains(&needle) || id.to_lowercase().contains(&needle)
        })
        .map(|(_, id)| id.to_string())
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        return check_full(&matches[0], listed);
    }
    let choices = if matches.is_empty() {
        entries
            .iter()
            .map(|(a, id)| format!("{a} → {id}"))
            .collect::<Vec<_>>()
    } else {
        matches.clone()
    };
    Err(format!("--model {input}: {}\ncandidates:\n  {}\nregister a model with: delegate tiers --alias name=provider/id",
        if matches.is_empty() { "model not registered" } else { "ambiguous model" }, choices.join("\n  ")))
}

pub fn unregistered_count(config: &Value, listed: &[PiModel]) -> usize {
    listed
        .iter()
        .filter(|m| {
            !config["models"]
                .as_object()
                .into_iter()
                .flatten()
                .any(|(_, id)| alias_model(id) == Some(&m.name))
        })
        .count()
}

fn check_full(id: &str, listed: &[PiModel]) -> Res<String> {
    validate_model(id)?;
    if !id.contains('/') {
        return Err(format!("{id}: alias must map to provider/id"));
    }
    if !listed.is_empty() && !listed.iter().any(|m| m.name == id) {
        return Err(format!(
            "{id} is not among Pi's models (pi --offline --list-models)"
        ));
    }
    Ok(id.into())
}

/// An environment override replaces this tier's entire rule list.
pub fn tier_rules(config: &Value, tier: &str) -> Vec<Value> {
    let env = setting(&format!("{}_MODEL", tier.to_uppercase()), "");
    let value = if env.is_empty() {
        config["tiers"][tier].clone()
    } else {
        json!(env)
    };
    match value {
        Value::String(m) if !m.is_empty() => vec![json!(m)],
        Value::Array(rules) => rules,
        _ => vec![],
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub index: usize,
    pub name: String,
    pub model: String,
    pub thinking: Option<String>,
}
impl Pick {
    pub fn value(&self) -> Value {
        json!({"index":self.index,"name":self.name,"model":self.model,"thinking":self.thinking})
    }
}

/// Environment > command line > rule > alias default > config > Pi's default.
pub fn thinking(cli: Option<&str>, rule: &Value, config: &Value) -> Res<Option<String>> {
    let env = setting("THINKING", "");
    let alias = selected_alias(rule.as_str().unwrap_or_else(|| s(rule, "model")), config);
    let value = (!env.is_empty())
        .then_some(env.as_str())
        .or(cli)
        .or_else(|| rule["thinking"].as_str())
        .or_else(|| config["models"][alias]["thinking"].as_str())
        .or_else(|| config["thinking"].as_str());
    if let Some(level) = value {
        validate_thinking(level)?;
    }
    Ok(value.map(str::to_string))
}

pub fn thinking_source(cli: Option<&str>, rule: &Value, config: &Value) -> &'static str {
    let alias = selected_alias(rule.as_str().unwrap_or_else(|| s(rule, "model")), config);
    if !setting("THINKING", "").is_empty() {
        "环境"
    } else if cli.is_some() {
        "CLI"
    } else if rule["thinking"].is_string() {
        "档位规则"
    } else if config["models"][alias]["thinking"].is_string() {
        "别名"
    } else if config["thinking"].is_string() {
        "配置"
    } else {
        "Pi 默认"
    }
}

pub fn candidate(rule: &Value, config: &Value, cli: Option<&str>, listed: &[PiModel]) -> Res<Pick> {
    let name = rule.as_str().unwrap_or_else(|| s(rule, "model"));
    Ok(Pick {
        index: 0,
        name: name.into(),
        model: resolve_model(name, config, listed)?,
        thinking: thinking(cli, rule, config)?,
    })
}

pub fn candidates(
    config: &Value,
    tier: &str,
    cli: Option<&str>,
    listed: &[PiModel],
) -> Res<Vec<Pick>> {
    tier_rules(config, tier)
        .iter()
        .enumerate()
        .map(|(index, r)| {
            candidate(r, config, cli, listed).map(|mut p| {
                p.index = index;
                p
            })
        })
        .collect()
}

/// Resolve time/capacity rules only; image attachments never change the model.
pub fn pick(
    config: &Value,
    tier: &str,
    cli: Option<&str>,
    listed: &[PiModel],
    now: Clock,
    running: &dyn Fn(&str) -> usize,
) -> Res<Pick> {
    let rules = tier_rules(config, tier);
    let all = candidates(config, tier, cli, listed)?;
    let index = rules
        .iter()
        .enumerate()
        .find(|(i, rule)| {
            rule.is_string()
                || agents::rule_matches(rule, now, &|_| {
                    if s(rule, "overflow") == "next" {
                        running(&all[*i].model)
                    } else {
                        0
                    }
                })
        })
        .map(|(i, _)| i)
        .ok_or_else(|| format!("tier {tier} has no model"))?;
    Ok(all[index].clone())
}

/// A resolved model has one machine-wide ceiling, including pinned starts and replies.
/// If several tiers declare it, the strictest cap wins; aliases cannot bypass it.
pub fn model_cap(config: &Value, model: &str) -> Res<Value> {
    let mut found = Value::Null;
    for tier in TIERS {
        for rule in config["tiers"][tier].as_array().into_iter().flatten() {
            if let Some(cap) = rule["maxActive"].as_u64() {
                let name = s(rule, "model");
                if resolve_model(name, config, &[])? == model
                    && found["limit"].as_u64().is_none_or(|n| cap < n)
                {
                    found = json!({"name":name,"limit":cap});
                }
            }
        }
    }
    Ok(found)
}

/// Guard only explicitly unsupported images; no fallback, and no Pi list means no check.
pub fn guard_images(model: &str, listed: &[PiModel]) -> Res<()> {
    if listed.iter().any(|m| m.name == model && !m.images) {
        return Err(format!("{model}: Pi reports images=no; add image to this model's input in Pi models.json, or use --model to choose an image-capable model"));
    }
    Ok(())
}

pub fn infer(read_only: bool) -> &'static str {
    if read_only {
        "review"
    } else {
        "write"
    }
}
pub fn inference_notice(tier: &str, name: &str, model: &str, thinking: Option<&str>) -> String {
    format!("delegate: 未指定 --tier，按{}推断为 {tier} 档 → {name}（{model}，thinking {}）；明确用途请写 --tier scout|review|write 或 --model <别名>",
        if tier == "write" { "写入" } else { "只读" }, thinking.unwrap_or("Pi 默认"))
}

/// Escalation advances within the captured tier list, never to another purpose.
pub fn next_candidate(current: &str, candidates: &[Value], index: usize) -> Option<Value> {
    let start = index.checked_add(1)?;
    if start > candidates.len() {
        return None;
    }
    candidates[start..]
        .iter()
        .find(|p| s(p, "model") != current)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn config() -> Value {
        json!({"models":{"sol":"provider/gpt-sol","ds":"provider/deepseek","spot":"spot/deepseek"},
            "thinking":"high", "tiers":{"scout":"ds","review":[{"model":"sol","maxActive":2,"thinking":"low"},"ds"],"write":"sol"}})
    }
    fn listed() -> Vec<PiModel> {
        parse_pi_models("provider model context max-out thinking images\nprovider gpt-sol 1M 32k yes yes\nprovider deepseek 64k 8k yes no\nspot deepseek 64k 8k no yes\nopenai builtin-no-key 128k 8k yes yes\n")
    }
    #[test]
    fn registry_exact_substring_unique_and_ambiguous() {
        let _guard = ENV.lock().unwrap();
        let c = config();
        let pi = listed();
        assert_eq!(resolve_model("sol", &c, &pi).unwrap(), "provider/gpt-sol");
        assert_eq!(
            resolve_model("provider/gpt-sol", &c, &pi).unwrap(),
            "provider/gpt-sol"
        );
        assert_eq!(resolve_model("SOL", &c, &pi).unwrap(), "provider/gpt-sol");
        assert_eq!(resolve_model("gpt-", &c, &pi).unwrap(), "provider/gpt-sol");
        let err = resolve_model("deepseek", &c, &pi).unwrap_err();
        for text in ["ambiguous model", "provider/deepseek", "spot/deepseek"] {
            assert!(err.contains(text), "{err}");
        }
        for input in ["openai/builtin-no-key", "builtin-no-key", "missing"] {
            let err = resolve_model(input, &c, &pi).unwrap_err();
            assert!(err.contains("not registered"));
            assert!(err.contains("--alias name=provider/id"));
        }
        assert_eq!(unregistered_count(&c, &pi), 1);
        assert_eq!(resolve_model("ds", &c, &[]).unwrap(), "provider/deepseek");
        assert!(resolve_model("sol", &c, &pi[1..])
            .unwrap_err()
            .contains("not among Pi"));
    }
    #[test]
    fn thinking_precedence_env_cli_rule_config_pi_default_and_suffix_errors() {
        let _guard = ENV.lock().unwrap();
        std::env::remove_var("DELEGATE_THINKING");
        let c = config();
        let rule = json!({"thinking":"low"});
        assert_eq!(thinking(None, &Value::Null, &json!({})).unwrap(), None);
        assert_eq!(
            thinking(None, &Value::Null, &c).unwrap().as_deref(),
            Some("high")
        );
        assert_eq!(thinking(None, &rule, &c).unwrap().as_deref(), Some("low"));
        assert_eq!(
            thinking(Some("medium"), &rule, &c).unwrap().as_deref(),
            Some("medium")
        );
        std::env::set_var("DELEGATE_THINKING", "off");
        assert_eq!(
            thinking(Some("medium"), &rule, &c).unwrap().as_deref(),
            Some("off")
        );
        std::env::remove_var("DELEGATE_THINKING");
        for model in ["sol:high", "provider/gpt-sol:low", "ds:max"] {
            assert!(resolve_model(model, &c, &listed())
                .unwrap_err()
                .contains("--thinking"));
        }
        assert!(validate_thinking("max").is_err());
        assert!(validate_model("provider/model:7b").is_ok());
    }
    #[test]
    fn rules_resolve_aliases_count_full_models_and_images_never_reroute() {
        let _guard = ENV.lock().unwrap();
        let mut c = config();
        c["tiers"]["review"][0]["overflow"] = json!("next");
        let pi = listed();
        let now = Clock {
            weekday: 1,
            minute: 600,
        };
        let pick = pick(&c, "review", None, &pi, now, &|_| 0).unwrap();
        assert_eq!(
            (&*pick.model, pick.thinking.as_deref()),
            ("provider/gpt-sol", Some("low"))
        );
        let pick = super::pick(&c, "review", None, &pi, now, &|id| {
            if id == "provider/gpt-sol" {
                2
            } else {
                0
            }
        })
        .unwrap();
        assert_eq!(pick.model, "provider/deepseek");
        assert_eq!(pi[0].context, "1M");
        assert!(pi[0].images);
        assert!(!pi[1].images);
        let error = guard_images(&pick.model, &pi).unwrap_err();
        for text in [
            "provider/deepseek",
            "images=no",
            "models.json",
            "input",
            "image",
            "--model",
        ] {
            assert!(error.contains(text));
        }
        assert!(guard_images(&pick.model, &[]).is_ok());
        assert!(guard_images("provider/gpt-sol", &pi).is_ok());
        std::env::set_var("DELEGATE_REVIEW_MODEL", "spot");
        assert_eq!(
            super::pick(&c, "review", None, &pi, now, &|_| 0)
                .unwrap()
                .model,
            "spot/deepseek"
        );
        std::env::remove_var("DELEGATE_REVIEW_MODEL");
    }
    #[test]
    fn alias_object_validation_and_resolution() {
        let _guard = ENV.lock().unwrap();
        let c = json!({"models":{"sol":{"model":"provider/gpt-sol","thinking":"medium"}}});
        crate::config::validate(&c, std::path::Path::new("config.json"), false).unwrap();
        assert_eq!(
            resolve_model("sol", &c, &listed()).unwrap(),
            "provider/gpt-sol"
        );
        assert_eq!(
            resolve_model("gpt-", &c, &listed()).unwrap(),
            "provider/gpt-sol"
        );
        assert_eq!(unregistered_count(&c, &listed()), 3);
    }
    #[test]
    fn alias_default_precedence() {
        let _guard = ENV.lock().unwrap();
        std::env::remove_var("DELEGATE_THINKING");
        let c = json!({"models":{"sol":{"model":"provider/gpt-sol","thinking":"medium"}},"thinking":"high"});
        let rule = json!("sol");
        assert_eq!(
            thinking(None, &rule, &c).unwrap().as_deref(),
            Some("medium")
        );
        assert_eq!(thinking_source(None, &rule, &c), "别名");
        assert_eq!(
            thinking(Some("low"), &rule, &c).unwrap().as_deref(),
            Some("low")
        );
        let rule = json!({"model":"sol","thinking":"off"});
        assert_eq!(thinking(None, &rule, &c).unwrap().as_deref(), Some("off"));
    }
    #[test]
    fn inference_notice_and_same_tier_next_candidate() {
        assert_eq!(infer(true), "review");
        assert_eq!(infer(false), "write");
        let notice = inference_notice("review", "sol", "provider/gpt-sol", Some("high"));
        assert!(notice.contains("按只读推断为 review 档 → sol（provider/gpt-sol，thinking high）"));
        assert!(notice.contains("--tier scout|review|write 或 --model"));
        let candidates = vec![
            json!({"model":"p/a","thinking":"low"}),
            json!({"model":"p/a"}),
            json!({"model":"p/b","thinking":"high"}),
        ];
        let next = next_candidate("p/a", &candidates, 0).unwrap();
        assert_eq!(s(&next, "model"), "p/b");
        assert_eq!(s(&next, "thinking"), "high");
        assert!(next_candidate("p/b", &candidates, 2).is_none());
        let repeated = vec![
            json!({"model":"p/a"}),
            json!({"model":"p/b"}),
            json!({"model":"p/a"}),
            json!({"model":"p/c"}),
        ];
        assert_eq!(
            s(&next_candidate("p/a", &repeated, 2).unwrap(), "model"),
            "p/c"
        );
    }
}
