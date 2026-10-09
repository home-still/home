use crate::cli::ConfigAction;
use crate::error::{PersonalError, Result};
use hs_common::CONFIG_REL_PATH;

pub async fn run(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let cfg = crate::config::Config::load()?;
            let yaml = serde_yaml_ng::to_string(&cfg)
                .map_err(|e| PersonalError::Config(format!("cannot serialize config: {e}")))?;
            println!("{yaml}");
        }
        ConfigAction::Path => {
            let home = dirs::home_dir().ok_or_else(|| {
                PersonalError::Config("could not determine home directory".into())
            })?;
            println!("{}", home.join(CONFIG_REL_PATH).display());
        }
    }
    Ok(())
}
