mod claude;
mod codex;
mod opencode;

use crate::{Provider, Session, pricing::Pricing};
use anyhow::Result;
use std::path::Path;

pub(crate) fn load(
    provider: Provider,
    path: &Path,
    pricing: &Pricing,
    warnings: &mut Vec<String>,
) -> Result<Vec<Session>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    match provider {
        Provider::Opencode => opencode::load(path, pricing, warnings),
        Provider::Claude => claude::load(path, pricing, warnings),
        Provider::Codex => codex::load(path, pricing, warnings),
    }
}
