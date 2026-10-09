use crate::config::Config;
use anyhow::{Context, Result};
use hs_common::global_args::GlobalArgs;

use crate::cli::ConfigAction;
use crate::output;

pub async fn run(action: ConfigAction, global: &GlobalArgs) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let mut config = Config::load().context("Failed to load config")?;
            // `show` goes to terminals, logs and bug reports: print that a key
            // is configured, never the key.
            let providers = &mut config.providers;
            for key in [
                &mut providers.openalex.api_key,
                &mut providers.semantic_scholar.api_key,
                &mut providers.core.api_key,
            ] {
                if key.is_some() {
                    *key = Some("<redacted>".to_string());
                }
            }
            if global.is_json() {
                output::print_json(&config)?;
            } else {
                let yaml =
                    serde_yaml_ng::to_string(&config).context("Failed to serialize config")?;
                println!("{yaml}")
            }
            Ok(())
        }
        ConfigAction::Path => {
            match Config::config_path() {
                Some(path) => {
                    if global.is_json() {
                        output::print_json(&serde_json::json!({
                            "path": path.display().to_string(),
                            "exists": path.exists(),
                        }))?;
                    } else {
                        println!("{}", path.display());
                    }
                }
                None => {
                    anyhow::bail!("Could not determine home directory");
                }
            }
            Ok(())
        }
    }
}
