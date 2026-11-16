pub mod display;
pub mod pricing;
mod providers;
mod util;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use pricing::Pricing;
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ValueEnum)]
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

#[derive(Debug, Default, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub provider: Provider,
    pub last_used: Option<DateTime<Utc>>,
    pub subagents: u64,
    #[serde(flatten)]
    pub usage: Usage,
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
            parent: None,
        }
    }
    pub(crate) fn touch(&mut self, time: Option<DateTime<Utc>>) {
        self.last_used = self.last_used.max(time);
    }
}

#[derive(Debug, Serialize)]
pub struct ProviderTotal {
    pub provider: Provider,
    pub sessions: u64,
    pub subagents: u64,
    #[serde(flatten)]
    pub usage: Usage,
}

#[derive(Debug, Serialize)]
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
            s
        });
        target.usage.merge(&session.usage);
        target.touch(session.last_used);
        if i != root {
            target.subagents += 1;
        }
    }
    output.into_values().collect()
}
