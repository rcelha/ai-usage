use crate::Tokens;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};

/// USD per million tokens. Cache writes are a separate category from input.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cache_write_1h: Option<f64>,
    #[serde(default)]
    pub above: Option<Box<Tier>>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub tokens: u64,
    #[serde(flatten)]
    pub rate: Rate,
}

#[derive(Debug, Clone)]
pub struct Pricing {
    rates: BTreeMap<String, Rate>,
}
impl Pricing {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut rates: BTreeMap<String, Rate> =
            serde_json::from_str(include_str!("../pricing.json"))?;
        if let Some(path) = path {
            let extra: BTreeMap<String, Rate> = serde_json::from_reader(
                std::fs::File::open(path)
                    .with_context(|| format!("opening prices {}", path.display()))?,
            )
            .with_context(|| format!("parsing prices {}", path.display()))?;
            rates.extend(extra);
        }
        for (name, rate) in &rates {
            validate(name, rate)?;
        }
        Ok(Self { rates })
    }

    pub(crate) fn estimate(&self, model: &str, tokens: Tokens, writes_1h: u64) -> Option<f64> {
        let rate = self.rates.get(model).or_else(|| {
            // Only strip an explicit YYYY-MM-DD or YYYYMMDD snapshot suffix.
            let base = if model.len() >= 11 && model.as_bytes()[model.len() - 11] == b'-' {
                let suffix = &model[model.len() - 10..];
                (suffix.as_bytes()[4] == b'-'
                    && suffix.as_bytes()[7] == b'-'
                    && suffix.replace('-', "").chars().all(|c| c.is_ascii_digit()))
                .then(|| &model[..model.len() - 11])
            } else {
                None
            };
            let base = base.or_else(|| {
                model.rsplit_once('-').and_then(|(base, suffix)| {
                    (suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()))
                        .then_some(base)
                })
            });
            base.and_then(|m| self.rates.get(m))
        })?;
        let prompt = tokens.input + tokens.cache_read + tokens.cache_write;
        let rate = rate
            .above
            .as_ref()
            .filter(|t| prompt > t.tokens)
            .map_or(rate, |t| &t.rate);
        let long = writes_1h.min(tokens.cache_write);
        Some(
            (tokens.input as f64 * rate.input
                + tokens.output as f64 * rate.output
                + tokens.cache_read as f64 * rate.cache_read
                + (tokens.cache_write - long) as f64 * rate.cache_write
                + long as f64 * rate.cache_write_1h.unwrap_or(rate.cache_write))
                / 1_000_000.0,
        )
    }
}

fn validate(name: &str, rate: &Rate) -> Result<()> {
    for value in [
        rate.input,
        rate.output,
        rate.cache_read,
        rate.cache_write,
        rate.cache_write_1h.unwrap_or(0.0),
    ] {
        ensure!(
            value.is_finite() && value >= 0.0,
            "invalid price for {name}: rates must be finite and nonnegative"
        );
    }
    if let Some(tier) = &rate.above {
        validate(name, &tier.rate)?;
    }
    Ok(())
}
