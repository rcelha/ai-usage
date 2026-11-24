pub mod display;
pub mod pricing;
mod providers;
pub mod server;
mod util;

use anyhow::{Result, anyhow};
use chrono::{DateTime, NaiveDate, Utc};
use clap::ValueEnum;
use pricing::Pricing;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Opencode,
    Claude,
    Codex,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Opencode => "opencode",
            Self::Claude => "claude",
            Self::Codex => "codex",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub opencode: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
}
impl Paths {
    pub fn discover() -> Result<Self> {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow!("cannot locate home directory; set HOME"))?;
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        Ok(Self {
            opencode: data.join("opencode"),
            claude: std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
            codex: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
        })
    }
}

/// Input excludes cache reads and writes. Output includes reasoning tokens.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}
impl std::ops::AddAssign for Tokens {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_read += rhs.cache_read;
        self.cache_write += rhs.cache_write;
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Usage {
    #[serde(flatten)]
    pub tokens: Tokens,
    pub requests: u64,
    pub known_cost_usd: f64,
    /// None if any usage lacks a known model price.
    pub cost_usd: Option<f64>,
    pub estimated: bool,
    pub unpriced_requests: u64,
    pub unpriced_models: BTreeSet<String>,
}
impl Usage {
    pub fn empty() -> Self {
        Self {
            cost_usd: Some(0.0),
            ..Self::default()
        }
    }
    pub(crate) fn record(
        &mut self,
        tokens: Tokens,
        cost: Option<f64>,
        estimated: bool,
        model: &str,
    ) {
        self.tokens += tokens;
        self.requests += 1;
        self.estimated |= estimated;
        if let Some(cost) = cost {
            self.known_cost_usd += cost;
        } else {
            self.unpriced_requests += 1;
            self.unpriced_models.insert(model.to_owned());
        }
        self.cost_usd = (self.unpriced_requests == 0).then_some(self.known_cost_usd);
    }
    pub(crate) fn merge(&mut self, other: &Self) {
        self.tokens += other.tokens;
        self.requests += other.requests;
        self.known_cost_usd += other.known_cost_usd;
        self.estimated |= other.estimated;
        self.unpriced_requests += other.unpriced_requests;
        self.unpriced_models
            .extend(other.unpriced_models.iter().cloned());
        self.cost_usd = (self.unpriced_requests == 0).then_some(self.known_cost_usd);
    }
}

/// Usage attributed to one model within a session, including its subagents.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelUsage {
    pub model: String,
    #[serde(flatten)]
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub provider: Provider,
    pub last_used: Option<DateTime<Utc>>,
    pub subagents: u64,
    #[serde(flatten)]
    pub usage: Usage,
    /// Per-model breakdown of `usage`, highest known cost first.
    pub models: Vec<ModelUsage>,
    #[serde(skip)]
    pub(crate) parent: Option<String>,
}
impl Session {
    pub(crate) fn new(id: String, provider: Provider) -> Self {
        Self {
            title: id.clone(),
            id,
            provider,
            last_used: None,
            subagents: 0,
            usage: Usage::empty(),
            models: Vec::new(),
            parent: None,
        }
    }
    fn model_usage(&mut self, model: &str) -> &mut Usage {
        let i = match self.models.iter().position(|m| m.model == model) {
            Some(i) => i,
            None => {
                self.models.push(ModelUsage {
                    model: model.to_owned(),
                    usage: Usage::empty(),
                });
                self.models.len() - 1
            }
        };
        &mut self.models[i].usage
    }
    pub(crate) fn record(
        &mut self,
        tokens: Tokens,
        cost: Option<f64>,
        estimated: bool,
        model: &str,
    ) {
        self.usage.record(tokens, cost, estimated, model);
        self.model_usage(model)
            .record(tokens, cost, estimated, model);
    }
    fn absorb(&mut self, other: &Session) {
        self.usage.merge(&other.usage);
        for m in &other.models {
            self.model_usage(&m.model).merge(&m.usage);
        }
        self.touch(other.last_used);
    }
    pub(crate) fn touch(&mut self, time: Option<DateTime<Utc>>) {
        self.last_used = self.last_used.max(time);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderTotal {
    pub provider: Provider,
    pub sessions: u64,
    pub subagents: u64,
    #[serde(flatten)]
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub sessions: Vec<Session>,
    pub totals: Usage,
    pub provider_totals: Vec<ProviderTotal>,
    pub warnings: Vec<String>,
}
impl Report {
    pub fn totals(sessions: &[Session]) -> Usage {
        let mut total = Usage::empty();
        for session in sessions {
            total.merge(&session.usage);
        }
        total
    }

    pub fn provider_totals(sessions: &[Session]) -> Vec<ProviderTotal> {
        [Provider::Opencode, Provider::Claude, Provider::Codex]
            .into_iter()
            .filter_map(|provider| {
                let mut total = ProviderTotal {
                    provider,
                    sessions: 0,
                    subagents: 0,
                    usage: Usage::empty(),
                };
                for session in sessions.iter().filter(|s| s.provider == provider) {
                    total.sessions += 1;
                    total.subagents += session.subagents;
                    total.usage.merge(&session.usage);
                }
                (total.sessions > 0).then_some(total)
            })
            .collect()
    }

    pub fn apply_filters(&mut self, since: Option<NaiveDate>, limit: Option<usize>) -> usize {
        if let Some(date) = since {
            self.sessions
                .retain(|s| s.last_used.is_some_and(|t| t.date_naive() >= date));
        }
        self.totals = Self::totals(&self.sessions);
        self.provider_totals = Self::provider_totals(&self.sessions);
        let matched = self.sessions.len();
        if let Some(limit) = limit {
            self.sessions.truncate(limit);
        }
        matched
    }

    /// Finds one session by exact ID, then unique ID prefix, then unique
    /// case-insensitive title substring.
    pub fn find_session(&self, query: &str) -> Result<&Session> {
        let query = query.trim();
        if query.is_empty() {
            return Err(anyhow!("session query is empty"));
        }
        let lower = query.to_lowercase();
        let tiers: [&dyn Fn(&Session) -> bool; 3] =
            [&|s| s.id == query, &|s| s.id.starts_with(query), &|s| {
                s.title.to_lowercase().contains(&lower)
            }];
        for matches in tiers {
            let found: Vec<&Session> = self.sessions.iter().filter(|s| matches(s)).collect();
            match found.as_slice() {
                [] => continue,
                [one] => return Ok(one),
                many => {
                    let mut message = format!(
                        "'{query}' matches {} sessions; use a full ID or add --provider:",
                        many.len()
                    );
                    for s in many.iter().take(10) {
                        let title: String = s
                            .title
                            .chars()
                            .map(|c| if c.is_control() { ' ' } else { c })
                            .take(60)
                            .collect();
                        message.push_str(&format!("\n  {}  {}  {title}", s.id, s.provider));
                    }
                    if many.len() > 10 {
                        message.push_str("\n  …");
                    }
                    return Err(anyhow!(message));
                }
            }
        }
        Err(anyhow!("no session matches '{query}'"))
    }
}

pub fn collect(paths: &Paths, providers: &[Provider], pricing: &Pricing) -> Report {
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    for (provider, path) in [
        (Provider::Opencode, &paths.opencode),
        (Provider::Claude, &paths.claude),
        (Provider::Codex, &paths.codex),
    ] {
        if !providers.is_empty() && !providers.contains(&provider) {
            continue;
        }
        match providers::load(provider, path, pricing, &mut warnings) {
            Ok(found) => sessions.extend(found),
            Err(e) => warnings.push(format!("{provider}: {e:#}")),
        }
    }
    let mut sessions = roll_up(sessions, &mut warnings);
    for session in &mut sessions {
        session.models.sort_by(|a, b| {
            b.usage
                .known_cost_usd
                .total_cmp(&a.usage.known_cost_usd)
                .then_with(|| b.usage.requests.cmp(&a.usage.requests))
                .then_with(|| a.model.cmp(&b.model))
        });
    }
    sessions.sort_by(|a, b| {
        b.last_used
            .cmp(&a.last_used)
            .then_with(|| a.provider.to_string().cmp(&b.provider.to_string()))
            .then_with(|| a.id.cmp(&b.id))
    });
    let totals = Report::totals(&sessions);
    let provider_totals = Report::provider_totals(&sessions);
    Report {
        sessions,
        totals,
        provider_totals,
        warnings,
    }
}

fn roll_up(sessions: Vec<Session>, warnings: &mut Vec<String>) -> Vec<Session> {
    let indices: HashMap<_, _> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| ((s.provider, s.id.clone()), i))
        .collect();
    let mut output: HashMap<usize, Session> = HashMap::new();
    for (i, session) in sessions.iter().enumerate() {
        let mut root = i;
        let mut visited = HashSet::from([i]);
        while let Some(parent) = &sessions[root].parent {
            let Some(&next) = indices.get(&(session.provider, parent.clone())) else {
                break;
            };
            if !visited.insert(next) {
                warnings.push(format!(
                    "{}: cyclic session ancestry near {}",
                    session.provider, session.id
                ));
                root = *visited.iter().min().unwrap();
                break;
            }
            root = next;
        }
        let target = output.entry(root).or_insert_with(|| {
            let mut s = sessions[root].clone();
            s.usage = Usage::empty();
            s.subagents = 0;
            s.models.clear();
            s
        });
        target.absorb(session);
        if i != root {
            target.subagents += 1;
        }
    }
    output.into_values().collect()
}
