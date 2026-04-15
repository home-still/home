use crate::config::Config;
use anyhow::{Context, Result};
use hs_common::global_args::GlobalArgs;

use crate::cli::ConfigAction;
use crate::output;

pub async fn run(action: ConfigAction, global: &GlobalArgs) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let config = Config::load().context("Failed to load config")?;
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
