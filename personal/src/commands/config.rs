use crate::cli::ConfigAction;
use crate::error::Result;
use hs_common::CONFIG_REL_PATH;

pub async fn run(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let cfg = crate::config::Config::load()?;
            println!(
                "{}",
                serde_yaml_ng::to_string(&cfg)
                    .unwrap_or_else(|_| String::from("(error serializing)"))
            );
        }
        ConfigAction::Path => {
            if let Some(home) = dirs::home_dir() {
                println!("{}", home.join(CONFIG_REL_PATH).display());
            }
        }
    }
    Ok(())
}
